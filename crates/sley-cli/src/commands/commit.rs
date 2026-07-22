//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used)]

use sley::plumbing::{sley_diff_merge, sley_object, sley_worktree};
// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use super::status::{
    StatusLongDisplay, SubmoduleIgnoreResolver, apply_submodule_ignore, build_status_long_sink,
    commit_comment_string, print_status_long, resolve_commit_comment_char,
    resolve_status_rename_config, status_comment_prefix, status_entries_have_index_changes,
    status_submodule_summary,
};
use crate::*;

pub(crate) enum PrepareCommitMsgSource<'a> {
    None,
    Message,
    Template,
    Merge,
    Commit(&'a str),
}

pub(crate) fn run_prepare_commit_msg_hook(
    git_dir: &Path,
    editmsg: &Path,
    source: PrepareCommitMsgSource<'_>,
    mut env: Vec<(String, String)>,
    set_no_editor_env: bool,
) -> Result<bool> {
    let editmsg_arg = editmsg.to_string_lossy().into_owned();
    let mut args = vec![editmsg_arg];
    match source {
        PrepareCommitMsgSource::None => {}
        PrepareCommitMsgSource::Message => args.push("message".to_string()),
        PrepareCommitMsgSource::Template => args.push("template".to_string()),
        PrepareCommitMsgSource::Merge => args.push("merge".to_string()),
        PrepareCommitMsgSource::Commit(rev) => {
            args.push("commit".to_string());
            args.push(rev.to_string());
        }
    }
    if set_no_editor_env {
        set_hook_env(&mut env, "GIT_EDITOR", ":");
    }
    match commands::hooks::run_hook_at(
        git_dir,
        "prepare-commit-msg",
        commands::hooks::HookRun {
            args,
            env,
            force_serial: true,
            ..commands::hooks::HookRun::default()
        },
    ) {
        Err(GitError::Exit(code)) => {
            eprintln!("error: prepare-commit-msg hook failed");
            Err(GitError::Exit(code))
        }
        result => result,
    }
}

fn set_hook_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, existing)) = env.iter_mut().find(|(existing, _)| existing == key) {
        *existing = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}

fn refresh_commit_selection_cache_tree(cli_session: &crate::session::CliSession) -> Result<()> {
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let odb = FileObjectDatabase::from_git_dir(&git_dir, format);
    sley_worktree::refresh_repository_cache_tree(&git_dir, format, &odb)
}

enum CommitShortFlag {
    /// A boolean flag that takes no value (e.g. `-q`, `-s`, `-a`).
    Boolean,
    /// A flag whose value is required (e.g. `-m`, `-F`, `-C`, `-c`, `-t`,
    /// `-U`). In a cluster it consumes the rest of the cluster; standalone it
    /// consumes the next argument.
    RequiresValue,
    /// A flag whose value is optional (`-S`, `-u`; `PARSE_OPT_OPTARG`). It
    /// consumes the rest of the cluster if any, but never the next argument.
    OptionalValue,
}

fn commit_short_flag_kind(ch: char) -> Option<CommitShortFlag> {
    match ch {
        // OPT__QUIET / OPT__VERBOSE and the plain OPT_BOOL entries.
        'q' | 'v' | 's' | 'e' | 'a' | 'i' | 'p' | 'o' | 'n' | 'z' => Some(CommitShortFlag::Boolean),
        // OPT_CALLBACK('m'), OPT_FILENAME('F'/'t'), OPT_STRING('c'/'C'),
        // OPT_DIFF_UNIFIED ('U').
        'm' | 'F' | 'c' | 'C' | 't' | 'U' => Some(CommitShortFlag::RequiresValue),
        // PARSE_OPT_OPTARG entries: gpg-sign ('S') and untracked-files ('u').
        'S' | 'u' => Some(CommitShortFlag::OptionalValue),
        _ => None,
    }
}

fn expand_commit_short_clusters(args: &[String]) -> Result<Vec<String>> {
    let mut expanded = Vec::with_capacity(args.len());
    let mut saw_dashdash = false;
    for arg in args {
        if saw_dashdash {
            expanded.push(arg.clone());
            continue;
        }
        if arg == "--" {
            saw_dashdash = true;
            expanded.push(arg.clone());
            continue;
        }
        let bytes = arg.as_bytes();
        // Not a short-option cluster: keep `-`, `--long`, and positionals as-is.
        if bytes.len() < 2 || bytes[0] != b'-' || bytes[1] == b'-' {
            expanded.push(arg.clone());
            continue;
        }
        let cluster = &arg[1..];
        let mut chars = cluster.char_indices();
        let Some((_, first)) = chars.next() else {
            expanded.push(arg.clone());
            continue;
        };
        // Only expand clusters that *start* with a boolean flag. If the first
        // flag is unknown or already takes a value, defer entirely to the main
        // parser (its glued-value / error arms own that input).
        if !matches!(
            commit_short_flag_kind(first),
            Some(CommitShortFlag::Boolean)
        ) {
            expanded.push(arg.clone());
            continue;
        }
        expanded.push(format!("-{first}"));
        // Walk the remaining flags in this cluster. A value-taking flag
        // swallows the rest of the cluster and ends the scan; the main parser
        // owns next-argument consumption when the glued value is empty.
        for (idx, ch) in chars {
            match commit_short_flag_kind(ch) {
                Some(CommitShortFlag::Boolean) => expanded.push(format!("-{ch}")),
                Some(CommitShortFlag::RequiresValue) | Some(CommitShortFlag::OptionalValue) => {
                    // `-q` `m` `rest` -> `-mrest`; when `rest` is empty we emit
                    // just `-m`, and the main parser consumes the next argument
                    // (required) or treats the value as absent (optional).
                    expanded.push(format!("-{}", &cluster[idx..]));
                    break;
                }
                None => {
                    // Unknown flag inside the cluster: preserve the existing
                    // error for the whole original cluster (exit 1) rather than
                    // emitting partial side effects from the leading flags.
                    return Err(GitError::Command(format!(
                        "unsupported commit argument {arg}; currently supports -m and -F"
                    )));
                }
            }
        }
    }
    Ok(expanded)
}

pub(crate) fn cmd_commit(
    cli_session: &crate::session::CliSession,
    raw_args: &[String],
) -> Result<()> {
    // `-h`/`--help` is handled by upstream's parse-options before any repo
    // state is consulted (so it works in a broken repository). Honour it first.
    if raw_args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return commit_usage();
    }
    let args = expand_commit_short_clusters(raw_args)?;
    let args = args.as_slice();
    let mut message_chunks = Vec::new();
    let mut file_message = None;
    let mut signoff = false;
    let mut quiet = false;
    let mut allow_empty = false;
    let mut allow_empty_message = false;
    let mut all = false;
    let mut author_override = None;
    let mut author_date = None;
    let mut reuse_message = None;
    let mut reedit_message = false;
    let mut fixup_commit = None;
    let mut squash_commit = None;
    // Raw `--trailer <arg>` strings, applied through the full interpret-trailers
    // engine (so per-token `trailer.*` config — key/where/ifexists/ifmissing/
    // command — applies, matching `git commit --trailer`).
    let mut trailers: Vec<String> = Vec::new();
    let mut reset_author = false;
    let mut amend = false;
    let mut verbose: i32 = -1;
    // `--status` / `--no-status` (and `commit.status` config) control whether the
    // working-tree status block is appended to the editor template (COMMIT_EDITMSG).
    // `None` = unset on the command line, so `commit.status` config (default true)
    // decides. Mirrors builtin/commit.c `include_status`.
    let mut include_status: Option<bool> = None;
    // The raw `--cleanup=<mode>` argument, if any. Resolution to a concrete mode
    // is deferred until `use_editor` is known (git: `default`/`scissors` depend
    // on whether an editor runs). `None` means "no --cleanup given" — fall back
    // to `commit.cleanup` config, then the editor-dependent default.
    let mut cleanup_arg: Option<String> = None;
    let mut include_without_paths = false;
    let mut only_without_paths = false;
    let mut status_mode = CommitStatusMode::Normal;
    let mut status_null = false;
    let mut null_implied_status = false;
    // `commit -u<mode>` / `--untracked-files=<mode>` overrides
    // `status.showUntrackedFiles` for the dry-run / status preview. `None` means
    // the flag was not given, so config / default applies.
    let mut commit_untracked: Option<sley_worktree::StatusUntrackedMode> = None;
    let mut dry_run = false;
    let mut no_verify = false;
    let mut no_post_rewrite = false;
    let mut interactive = false;
    let mut patch = false;
    let mut gpg_sign = false;
    let mut gpg_sign_key: Option<String> = None;
    let mut no_gpg_sign = false;
    let mut unified_context: Option<i64> = None;
    let mut inter_hunk_context: Option<i64> = None;
    let mut pathspec_from_file = None;
    let mut pathspec_from_file_active = false;
    let mut pathspec_file_nul = false;
    let mut pathspec_args = Vec::new();
    let mut edit_flag: Option<bool> = None;
    // `-t <file>` / `--template <file>`: the lowest-priority message body source.
    // Unlike `-m`/`-F`/`-C`, it does NOT suppress the editor (git keeps
    // `use_editor = 1`), so the user always edits the template.
    let mut template_file: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-m" => {
                let Some(message) = iter.next() else {
                    return commit_message_requires_value_error();
                };
                message_chunks.push(commit_message_arg_chunk(message));
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                message_chunks.push(commit_message_arg_chunk(&value[2..]));
            }
            value if value.starts_with("-am") => {
                all = true;
                let message = if value.len() > 3 {
                    &value[3..]
                } else {
                    let Some(message) = iter.next() else {
                        return commit_message_requires_value_error();
                    };
                    message
                };
                message_chunks.push(commit_message_arg_chunk(message));
            }
            "--message" => {
                let Some(message) = iter.next() else {
                    return commit_message_requires_value_error();
                };
                message_chunks.push(commit_message_arg_chunk(message));
            }
            value if value.starts_with("--message=") => {
                message_chunks.push(commit_message_arg_chunk(&value["--message=".len()..]));
            }
            "--no-message" => message_chunks.clear(),
            value if value.starts_with("--no-message=") => {
                return commit_option_takes_no_value_error("no-message");
            }
            "-F" | "--file" => {
                let Some(path) = iter.next() else {
                    return commit_tree_file_requires_value_error();
                };
                file_message = Some(if fixup_commit.is_some() {
                    Vec::new()
                } else {
                    read_porcelain_commit_message_file(path)?
                });
            }
            value if value.starts_with("-F") && value.len() > 2 => {
                file_message = Some(if fixup_commit.is_some() {
                    Vec::new()
                } else {
                    read_porcelain_commit_message_file(&value[2..])?
                });
            }
            value if value.starts_with("--file=") => {
                file_message = Some(if fixup_commit.is_some() {
                    Vec::new()
                } else {
                    read_porcelain_commit_message_file(&value["--file=".len()..])?
                });
            }
            "--no-file" => {}
            value if value.starts_with("--no-file=") => {
                return commit_option_takes_no_value_error("no-file");
            }
            "-C" | "--reuse-message" => {
                let Some(value) = iter.next() else {
                    return commit_reuse_message_requires_value_error(arg == "-C", false);
                };
                reuse_message = Some(value.to_string());
                reedit_message = false;
            }
            value if value.starts_with("-C") && value.len() > 2 => {
                reuse_message = Some(value[2..].to_string());
                reedit_message = false;
            }
            value if value.starts_with("--reuse-message=") => {
                reuse_message = Some(value["--reuse-message=".len()..].to_string());
                reedit_message = false;
            }
            "--no-reuse-message" => {
                reuse_message = None;
                reedit_message = false;
            }
            value if value.starts_with("--no-reuse-message=") => {
                return commit_option_takes_no_value_error("no-reuse-message");
            }
            "-c" | "--reedit-message" => {
                let Some(value) = iter.next() else {
                    return commit_reuse_message_requires_value_error(arg == "-c", true);
                };
                reuse_message = Some(value.to_string());
                reedit_message = true;
            }
            value if value.starts_with("-c") && value.len() > 2 => {
                reuse_message = Some(value[2..].to_string());
                reedit_message = true;
            }
            value if value.starts_with("--reedit-message=") => {
                reuse_message = Some(value["--reedit-message=".len()..].to_string());
                reedit_message = true;
            }
            "--no-reedit-message" => {
                reuse_message = None;
                reedit_message = false;
            }
            value if value.starts_with("--no-reedit-message=") => {
                return commit_option_takes_no_value_error("no-reedit-message");
            }
            "--fixup" => {
                let Some(value) = iter.next() else {
                    return commit_fixup_requires_value_error();
                };
                fixup_commit = Some(CommitFixup::parse(value)?);
            }
            value if value.starts_with("--fixup=") => {
                fixup_commit = Some(CommitFixup::parse(&value["--fixup=".len()..])?);
            }
            "--no-fixup" => fixup_commit = None,
            value if value.starts_with("--no-fixup=") => {
                return commit_option_takes_no_value_error("no-fixup");
            }
            "--squash" => {
                let Some(value) = iter.next() else {
                    return commit_squash_requires_value_error();
                };
                squash_commit = Some(value.to_string());
            }
            value if value.starts_with("--squash=") => {
                squash_commit = Some(value["--squash=".len()..].to_string());
            }
            "--no-squash" => squash_commit = None,
            value if value.starts_with("--no-squash=") => {
                return commit_option_takes_no_value_error("no-squash");
            }
            "--trailer" => {
                let Some(value) = iter.next() else {
                    return commit_trailer_requires_value_error();
                };
                trailers.push(value.clone());
            }
            value if value.starts_with("--trailer=") => {
                trailers.push(value["--trailer=".len()..].to_string());
            }
            "--no-trailer" => trailers.clear(),
            value if value.starts_with("--no-trailer=") => {
                return commit_option_takes_no_value_error("no-trailer");
            }
            "--reset-author" => reset_author = true,
            "--no-reset-author" => reset_author = false,
            value if value.starts_with("--reset-author=") => {
                return commit_option_takes_no_value_error("reset-author");
            }
            value if value.starts_with("--no-reset-author=") => {
                return commit_option_takes_no_value_error("no-reset-author");
            }
            "--amend" => amend = true,
            "--no-amend" => amend = false,
            value if value.starts_with("--amend=") => {
                return commit_option_takes_no_value_error("amend");
            }
            value if value.starts_with("--no-amend=") => {
                return commit_option_takes_no_value_error("no-amend");
            }
            "-s" | "--signoff" => signoff = true,
            "--no-signoff" => signoff = false,
            value if value.starts_with("--signoff=") => {
                return commit_option_takes_no_value_error("signoff");
            }
            value if value.starts_with("--no-signoff=") => {
                return commit_option_takes_no_value_error("no-signoff");
            }
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            value if value.starts_with("--quiet=") => {
                return commit_option_takes_no_value_error("quiet");
            }
            value if value.starts_with("--no-quiet=") => {
                return commit_option_takes_no_value_error("no-quiet");
            }
            "-a" | "--all" => all = true,
            "--no-all" => all = false,
            value if value.starts_with("--all=") => {
                return commit_option_takes_no_value_error("all");
            }
            value if value.starts_with("--no-all=") => {
                return commit_option_takes_no_value_error("no-all");
            }
            "--allow-empty" => allow_empty = true,
            "--no-allow-empty" => allow_empty = false,
            "--allow-empty-message" => allow_empty_message = true,
            "--no-allow-empty-message" => allow_empty_message = false,
            value if value.starts_with("--allow-empty=") => {
                return commit_option_takes_no_value_error("allow-empty");
            }
            value if value.starts_with("--no-allow-empty=") => {
                return commit_option_takes_no_value_error("no-allow-empty");
            }
            value if value.starts_with("--allow-empty-message=") => {
                return commit_option_takes_no_value_error("allow-empty-message");
            }
            value if value.starts_with("--no-allow-empty-message=") => {
                return commit_option_takes_no_value_error("no-allow-empty-message");
            }
            "--author" => {
                let Some(author) = iter.next() else {
                    return commit_author_requires_value_error();
                };
                author_override = Some(author.to_string());
            }
            value if value.starts_with("--author=") => {
                author_override = Some(value["--author=".len()..].to_string());
            }
            "--no-author" => author_override = None,
            value if value.starts_with("--no-author=") => {
                return commit_option_takes_no_value_error("no-author");
            }
            "--date" => {
                let Some(date) = iter.next() else {
                    return commit_date_requires_value_error();
                };
                author_date = Some(date.to_string());
            }
            value if value.starts_with("--date=") => {
                author_date = Some(value["--date=".len()..].to_string());
            }
            "--no-date" => author_date = None,
            value if value.starts_with("--no-date=") => {
                return commit_option_takes_no_value_error("no-date");
            }
            "-n" | "--no-verify" => no_verify = true,
            "--verify" => no_verify = false,
            value if value.starts_with("--no-verify=") => {
                return commit_option_takes_no_value_error("no-verify");
            }
            value if value.starts_with("--verify=") => {
                return commit_option_takes_no_value_error("no-no-verify");
            }
            "-S" | "--gpg-sign" => {
                gpg_sign = true;
                gpg_sign_key = None;
                no_gpg_sign = false;
            }
            value if value.starts_with("-S") && value.len() > 2 => {
                gpg_sign = true;
                gpg_sign_key = Some(value[2..].to_string());
                no_gpg_sign = false;
            }
            value if value.starts_with("--gpg-sign=") => {
                gpg_sign = true;
                gpg_sign_key = Some(value["--gpg-sign=".len()..].to_string());
                no_gpg_sign = false;
            }
            "--no-gpg-sign" => {
                gpg_sign = false;
                gpg_sign_key = None;
                no_gpg_sign = true;
            }
            value if value.starts_with("--no-gpg-sign=") => {
                return commit_option_takes_no_value_error("no-gpg-sign");
            }
            "--post-rewrite" => no_post_rewrite = false,
            "--no-post-rewrite" => no_post_rewrite = true,
            value if value.starts_with("--post-rewrite=") => {
                return commit_option_takes_no_value_error("no-no-post-rewrite");
            }
            value if value.starts_with("--no-post-rewrite=") => {
                return commit_option_takes_no_value_error("no-post-rewrite");
            }
            "--status" => include_status = Some(true),
            "--no-status" => include_status = Some(false),
            value if value.starts_with("--status=") => {
                return commit_option_takes_no_value_error("status");
            }
            value if value.starts_with("--no-status=") => {
                return commit_option_takes_no_value_error("no-status");
            }
            "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            value if value.starts_with("--dry-run=") => {
                return commit_option_takes_no_value_error("dry-run");
            }
            value if value.starts_with("--no-dry-run=") => {
                return commit_option_takes_no_value_error("no-dry-run");
            }
            "--short" => {
                status_mode = CommitStatusMode::Short;
                null_implied_status = false;
            }
            "--no-short" => {
                if status_mode == CommitStatusMode::Short {
                    status_mode = CommitStatusMode::Normal;
                }
                null_implied_status = false;
            }
            value if value.starts_with("--short=") => {
                return commit_option_takes_no_value_error("short");
            }
            value if value.starts_with("--no-short=") => {
                return commit_option_takes_no_value_error("no-short");
            }
            "--porcelain" => {
                status_mode = CommitStatusMode::Porcelain;
                null_implied_status = false;
            }
            "--no-porcelain" => {
                if status_mode == CommitStatusMode::Porcelain {
                    status_mode = CommitStatusMode::Normal;
                }
                null_implied_status = false;
            }
            value if value.starts_with("--porcelain=") => {
                return commit_option_takes_no_value_error("porcelain");
            }
            value if value.starts_with("--no-porcelain=") => {
                return commit_option_takes_no_value_error("no-porcelain");
            }
            "-z" | "--null" => {
                if status_mode == CommitStatusMode::Normal {
                    status_mode = CommitStatusMode::Short;
                    null_implied_status = true;
                }
                status_null = true;
            }
            "--no-null" => {
                status_null = false;
                if null_implied_status {
                    status_mode = CommitStatusMode::Normal;
                    null_implied_status = false;
                }
            }
            value if value.starts_with("--null=") => {
                return commit_option_takes_no_value_error("null");
            }
            value if value.starts_with("--no-null=") => {
                return commit_option_takes_no_value_error("no-null");
            }
            "--long" => {
                status_mode = CommitStatusMode::Long;
                null_implied_status = false;
            }
            "--no-long" => {
                if status_mode == CommitStatusMode::Long {
                    status_mode = CommitStatusMode::Normal;
                }
                null_implied_status = false;
            }
            value if value.starts_with("--long=") => {
                return commit_option_takes_no_value_error("long");
            }
            value if value.starts_with("--no-long=") => {
                return commit_option_takes_no_value_error("no-long");
            }
            "--ahead-behind" | "--no-ahead-behind" => {}
            value if value.starts_with("--ahead-behind=") => {
                return commit_option_takes_no_value_error("ahead-behind");
            }
            value if value.starts_with("--no-ahead-behind=") => {
                return commit_option_takes_no_value_error("no-ahead-behind");
            }
            "--interactive" => interactive = true,
            "--no-interactive" => interactive = false,
            value if value.starts_with("--interactive=") => {
                return commit_option_takes_no_value_error("interactive");
            }
            value if value.starts_with("--no-interactive=") => {
                return commit_option_takes_no_value_error("no-interactive");
            }
            "-p" | "--patch" => patch = true,
            "--no-patch" => patch = false,
            value if value.starts_with("--patch=") => {
                return commit_option_takes_no_value_error("patch");
            }
            value if value.starts_with("--no-patch=") => {
                return commit_option_takes_no_value_error("no-patch");
            }
            "-U" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(true);
                };
                patch_validate_unified_context(value, true)?;
                unified_context = value.parse::<i64>().ok();
            }
            value if value.starts_with("-U") && value.len() > 2 => {
                let value = &value[2..];
                patch_validate_unified_context(value, true)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--unified" => {
                let Some(value) = iter.next() else {
                    return commit_unified_requires_value_error(false);
                };
                patch_validate_unified_context(value, false)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--unified=" => {
                return commit_unified_expects_numerical_value_error(false);
            }
            value if value.starts_with("--unified=") => {
                let value = &value["--unified=".len()..];
                patch_validate_unified_context(value, false)?;
                unified_context = value.parse::<i64>().ok();
            }
            "--inter-hunk-context" => {
                let Some(value) = iter.next() else {
                    return commit_inter_hunk_context_requires_value_error();
                };
                patch_validate_inter_hunk_context(value)?;
                inter_hunk_context = value.parse::<i64>().ok();
            }
            "--inter-hunk-context=" => {
                return commit_inter_hunk_context_expects_numerical_value_error();
            }
            value if value.starts_with("--inter-hunk-context=") => {
                let value = &value["--inter-hunk-context=".len()..];
                patch_validate_inter_hunk_context(value)?;
                inter_hunk_context = value.parse::<i64>().ok();
            }
            "-v" | "--verbose" => verbose = verbose.max(0).saturating_add(1),
            "--no-verbose" => verbose = 0,
            value if value.starts_with("--verbose=") => {
                return commit_option_takes_no_value_error("verbose");
            }
            value if value.starts_with("--no-verbose=") => {
                return commit_option_takes_no_value_error("no-verbose");
            }
            "-u" | "-unormal" | "--untracked-files" => {
                commit_untracked = Some(sley_worktree::StatusUntrackedMode::Normal);
            }
            "-uno" => commit_untracked = Some(sley_worktree::StatusUntrackedMode::None),
            "-uall" => commit_untracked = Some(sley_worktree::StatusUntrackedMode::All),
            value if value.starts_with("-u") && value.len() > 2 => {
                return commit_invalid_untracked_files_mode_error(&value[2..]);
            }
            value if value.starts_with("--untracked-files=") => {
                let mode = &value["--untracked-files=".len()..];
                commit_untracked = Some(match mode {
                    "no" => sley_worktree::StatusUntrackedMode::None,
                    "normal" => sley_worktree::StatusUntrackedMode::Normal,
                    "all" => sley_worktree::StatusUntrackedMode::All,
                    _ => return commit_invalid_untracked_files_mode_error(mode),
                });
            }
            "--no-untracked-files" => {
                commit_untracked = Some(sley_worktree::StatusUntrackedMode::None);
            }
            value if value.starts_with("--no-untracked-files=") => {
                return commit_option_takes_no_value_error("no-untracked-files");
            }
            "--pathspec-from-file" => {
                let Some(value) = iter.next() else {
                    return commit_pathspec_from_file_requires_value_error();
                };
                pathspec_from_file = Some(value.to_string());
                pathspec_from_file_active = true;
            }
            value if value.starts_with("--pathspec-from-file=") => {
                pathspec_from_file = Some(value["--pathspec-from-file=".len()..].to_string());
                pathspec_from_file_active = true;
            }
            "--no-pathspec-from-file" => {}
            value if value.starts_with("--no-pathspec-from-file=") => {
                return commit_option_takes_no_value_error("no-pathspec-from-file");
            }
            "--pathspec-file-nul" => pathspec_file_nul = true,
            "--no-pathspec-file-nul" => pathspec_file_nul = false,
            value if value.starts_with("--pathspec-file-nul=") => {
                return commit_option_takes_no_value_error("pathspec-file-nul");
            }
            value if value.starts_with("--no-pathspec-file-nul=") => {
                return commit_option_takes_no_value_error("no-pathspec-file-nul");
            }
            "-i" | "--include" => include_without_paths = true,
            "--no-include" => include_without_paths = false,
            value if value.starts_with("--include=") => {
                return commit_option_takes_no_value_error("include");
            }
            value if value.starts_with("--no-include=") => {
                return commit_option_takes_no_value_error("no-include");
            }
            "-o" | "--only" => only_without_paths = true,
            "--no-only" => only_without_paths = false,
            value if value.starts_with("--only=") => {
                return commit_option_takes_no_value_error("only");
            }
            value if value.starts_with("--no-only=") => {
                return commit_option_takes_no_value_error("no-only");
            }
            "-e" | "--edit" => edit_flag = Some(true),
            "--no-edit" => edit_flag = Some(false),
            value if value.starts_with("--edit=") => {
                return commit_option_takes_no_value_error("edit");
            }
            value if value.starts_with("--no-edit=") => {
                return commit_option_takes_no_value_error("no-edit");
            }
            "--branch" | "--no-branch" => {}
            value if value.starts_with("--branch=") => {
                return commit_option_takes_no_value_error("branch");
            }
            value if value.starts_with("--no-branch=") => {
                return commit_option_takes_no_value_error("no-branch");
            }
            "-t" => {
                let Some(template) = iter.next() else {
                    return commit_template_short_requires_value_error();
                };
                template_file = Some(template.clone());
            }
            value if value.starts_with("-t") && value.len() > 2 => {
                template_file = Some(value[2..].to_string());
            }
            "--template" => {
                let Some(template) = iter.next() else {
                    return commit_template_requires_value_error();
                };
                template_file = Some(template.clone());
            }
            value if let Some(path) = value.strip_prefix("--template=") => {
                template_file = Some(path.to_string());
            }
            "--no-template" => template_file = None,
            value if value.starts_with("--no-template=") => {
                return commit_option_takes_no_value_error("no-template");
            }
            "--cleanup" => {
                let Some(value) = iter.next() else {
                    return commit_cleanup_requires_value_error();
                };
                // Validate eagerly (git rejects a bad mode at parse time) but
                // defer resolution until `use_editor` is known.
                validate_commit_cleanup_mode(value)?;
                cleanup_arg = Some(value.clone());
            }
            value if value.starts_with("--cleanup=") => {
                let arg = &value["--cleanup=".len()..];
                validate_commit_cleanup_mode(arg)?;
                cleanup_arg = Some(arg.to_string());
            }
            "--no-cleanup" => cleanup_arg = Some("whitespace".to_string()),
            value if value.starts_with("--no-cleanup=") => {
                return commit_option_takes_no_value_error("no-cleanup");
            }
            "--" => {
                if pathspec_from_file_active && !iter.as_slice().is_empty() {
                    return commit_pathspec_from_file_with_inline_pathspec_error();
                }
                pathspec_args.extend(iter.by_ref().cloned());
            }
            value => {
                if value.starts_with('-') {
                    if pathspec_from_file_active {
                        return commit_pathspec_from_file_with_inline_pathspec_error();
                    }
                    return Err(GitError::Command(format!(
                        "unsupported commit argument {value}; currently supports -m and -F"
                    )));
                }
                if pathspec_from_file_active {
                    return commit_pathspec_from_file_with_inline_pathspec_error();
                }
                pathspec_args.push(value.to_string());
            }
        }
    }
    if reuse_message.is_some() && !message_chunks.is_empty() {
        let option = if reedit_message { "-c" } else { "-C" };
        eprintln!("fatal: options '-m' and '{option}' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if reuse_message.is_some() && file_message.is_some() {
        let option = if reedit_message { "-c" } else { "-C" };
        eprintln!("fatal: options '{option}' and '-F' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if fixup_commit.is_some() && reuse_message.is_some() {
        let option = if reedit_message { "-c" } else { "-C" };
        eprintln!("fatal: options '{option}' and '--fixup' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if let Some(fixup) = &fixup_commit
        && fixup.is_amend_style()
        && !message_chunks.is_empty()
    {
        let option = if fixup.is_reword() {
            "--fixup:reword"
        } else {
            "--fixup:amend"
        };
        eprintln!("fatal: options '-m' and '{option}' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if squash_commit.is_some() && fixup_commit.is_some() {
        eprintln!("fatal: options '--squash' and '--fixup' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if let Some(fixup) = &fixup_commit
        && fixup.is_reword()
    {
        if !pathspec_args.is_empty() {
            eprintln!(
                "fatal: reword option of '--fixup' and path '{}' cannot be used together",
                pathspec_args[0]
            );
            return Err(GitError::Exit(128));
        }
        if all || include_without_paths || only_without_paths || interactive || patch {
            eprintln!(
                "fatal: reword option of '--fixup' and '--patch/--interactive/--all/--include/--only' cannot be used together"
            );
            return Err(GitError::Exit(128));
        }
    }
    if fixup_commit.is_some() && file_message.is_some() {
        eprintln!("fatal: options '-F' and '--fixup' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if file_message.is_some() && !message_chunks.is_empty() {
        eprintln!("fatal: options '-m' and '-F' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if pathspec_from_file_active && (interactive || patch) {
        eprintln!(
            "fatal: options '--pathspec-from-file' and '--interactive/--patch' cannot be used together"
        );
        return Err(GitError::Exit(128));
    }
    if pathspec_from_file_active && all {
        eprintln!("fatal: options '--pathspec-from-file' and '-a' cannot be used together");
        return Err(GitError::Exit(128));
    }
    // git: die only when no paths and (--include, or --only without --amend and
    // without --allow-empty). `git commit --allow-empty --only` is valid.
    let amend_style = amend
        || fixup_commit
            .as_ref()
            .is_some_and(CommitFixup::is_amend_style);
    if pathspec_args.is_empty()
        && pathspec_from_file.is_none()
        && (include_without_paths || (only_without_paths && !amend_style && !allow_empty))
    {
        eprintln!("fatal: No paths with --include/--only does not make sense.");
        return Err(GitError::Exit(128));
    }
    if pathspec_file_nul && pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        return Err(GitError::Exit(128));
    }
    if let Some(pathspec_file) = pathspec_from_file.as_deref() {
        let pathspecs =
            read_commit_pathspecs_from_file(Path::new(pathspec_file), pathspec_file_nul)?;
        if pathspec_from_file_active {
            pathspec_args.extend(
                pathspecs
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned()),
            );
        }
    }
    if pathspec_from_file_active
        && pathspec_args.is_empty()
        && (include_without_paths || only_without_paths)
    {
        eprintln!("fatal: No paths with --include/--only does not make sense.");
        return Err(GitError::Exit(128));
    }
    if !pathspec_args.is_empty() {
        if all {
            eprintln!(
                "fatal: paths '{} ...' with -a does not make sense",
                pathspec_args[0]
            );
            return Err(GitError::Exit(128));
        }
    }
    if unified_context.is_some() && !interactive && !patch {
        eprintln!("fatal: the option '--unified' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if inter_hunk_context.is_some() && !interactive && !patch {
        eprintln!("fatal: the option '--inter-hunk-context' requires '--interactive/--patch'");
        return Err(GitError::Exit(128));
    }
    if status_mode != CommitStatusMode::Normal {
        return cmd_commit_status_preview(
            cli_session,
            status_mode,
            status_null,
            amend,
            commit_untracked,
        );
    }
    if dry_run {
        return cmd_commit_long_status_preview(cli_session, amend, commit_untracked);
    }
    if interactive && !patch {
        commands::add_interactive::cmd_add_interactive(cli_session, &pathspec_args)?;
        refresh_commit_selection_cache_tree(cli_session)?;
    }
    if patch {
        commands::add_interactive::cmd_add_patch(
            cli_session,
            &pathspec_args,
            unified_context,
            inter_hunk_context,
            true,
        )?;
        refresh_commit_selection_cache_tree(cli_session)?;
        if template_file.is_none()
            && file_message.is_none()
            && message_chunks.is_empty()
            && reuse_message.is_none()
        {
            return Ok(());
        }
    }
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let repo_config = read_repo_config(&git_dir).ok();
    let identity_config = identity_effective_config_for(cli_session)
        .or_else(|| repo_config.clone())
        .unwrap_or_default();
    if !gpg_sign && !no_gpg_sign {
        gpg_sign = repo_config
            .as_ref()
            .and_then(|config| config.get_bool("commit", None, "gpgsign"))
            .unwrap_or(false);
    }
    if template_file.is_none()
        && file_message.is_none()
        && message_chunks.is_empty()
        && reuse_message.is_none()
        && fixup_commit.is_none()
    {
        template_file = repo_config
            .as_ref()
            .and_then(|config| config.get("commit", None, "template").map(str::to_string));
    }
    if verbose < 0 {
        verbose = repo_config
            .as_ref()
            .and_then(|config| commit_verbose_config(config.get("commit", None, "verbose")))
            .unwrap_or(0);
    }
    let commit_odb = FileObjectDatabase::from_git_dir(&git_dir, format);
    let commit_refs = FileRefStore::new(&git_dir, format);
    author_override = resolve_commit_author_nickname(
        &commit_refs,
        &commit_odb,
        format,
        author_override.as_deref(),
        cli_session.replace_objects(),
    )?;
    let in_merge = git_dir.join("MERGE_HEAD").is_file();
    let in_cherry_pick = git_dir.join("CHERRY_PICK_HEAD").is_file();
    let in_revert = git_dir.join("REVERT_HEAD").is_file();
    let in_rebase = rebase_in_progress(&git_dir);
    if reset_author && reuse_message.is_none() && !amend && !in_cherry_pick && !in_rebase {
        eprintln!("fatal: --reset-author can be used only with -C, -c or --amend.");
        return Err(GitError::Exit(128));
    }
    if !pathspec_args.is_empty() {
        if in_rebase {
            eprintln!("fatal: cannot do a partial commit during a rebase.");
            return Err(GitError::Exit(128));
        }
        if in_merge {
            eprintln!("fatal: cannot do a partial commit during a merge.");
            return Err(GitError::Exit(128));
        }
        if in_cherry_pick || in_revert {
            eprintln!("fatal: cannot do a partial commit during a cherry-pick.");
            return Err(GitError::Exit(128));
        }
    }
    if amend {
        if in_rebase {
            eprintln!("fatal: You are in the middle of a rebase -- cannot amend.");
            return Err(GitError::Exit(128));
        }
        if in_merge {
            eprintln!("fatal: You are in the middle of a merge -- cannot amend.");
            return Err(GitError::Exit(128));
        }
        if in_cherry_pick || in_revert {
            eprintln!("fatal: You are in the middle of a cherry-pick -- cannot amend.");
            return Err(GitError::Exit(128));
        }
    }
    // `i18n.commitEncoding` is recorded as the commit's `encoding` header so that
    // `git log` can re-encode the message to the log output encoding (UTF-8 by
    // default). git omits the header for UTF-8. Use the `-c`-aware config loaded
    // above — `commit_encoding_config` reads disk-only and drops CLI overrides.
    let commit_encoding = repo_config
        .as_ref()
        .and_then(|config| {
            config
                .get("i18n", None, "commitEncoding")
                .map(str::to_string)
        })
        .unwrap_or_else(|| "UTF-8".to_string());
    let commit_encoding_header =
        (!encoding_is_utf8(&commit_encoding)).then(|| commit_encoding.clone().into_bytes());
    let committer = commit_identity_from_env("COMMITTER", &identity_config)?;
    let amended_old_oid = if amend {
        commands::merge_rebase::head_commit_oid(&commit_refs)?
    } else {
        None
    };
    let amended_commit = amend
        .then(|| read_amended_commit(&git_dir, format))
        .transpose()?;
    let reused_commit = reuse_message
        .as_deref()
        .map(|rev| read_reused_commit(&git_dir, format, rev, cli_session.replace_objects()))
        .transpose()?;
    let fixup_message = fixup_commit
        .as_ref()
        .map(|fixup| {
            read_fixup_commit_message(
                &git_dir,
                format,
                fixup,
                &commit_encoding,
                cli_session.replace_objects(),
            )
        })
        .transpose()?;
    let fixup_reword_tree = if fixup_commit
        .as_ref()
        .is_some_and(|fixup| fixup.is_reword() || (fixup.is_amend_style() && only_without_paths))
    {
        let Some(commit) = read_head_commit(&git_dir, format)? else {
            eprintln!("fatal: You have nothing to amend.");
            return Err(GitError::Exit(128));
        };
        Some(commit.tree)
    } else {
        None
    };
    let squash_message = squash_commit
        .as_deref()
        .map(|rev| {
            read_squash_commit_message(
                &git_dir,
                format,
                rev,
                &commit_encoding,
                cli_session.replace_objects(),
            )
        })
        .transpose()?;
    let author = if reset_author {
        build_commit_author_identity(
            author_override.as_deref(),
            author_date.as_deref(),
            &identity_config,
        )?
    } else if let Some(commit) = &reused_commit {
        build_reused_commit_author_identity(
            &commit.author,
            author_override.as_deref(),
            author_date.as_deref(),
        )?
    } else if let Some(commit) = &amended_commit {
        build_reused_commit_author_identity(
            &commit.author,
            author_override.as_deref(),
            author_date.as_deref(),
        )?
    } else {
        build_commit_author_identity(
            author_override.as_deref(),
            author_date.as_deref(),
            &identity_config,
        )?
    };
    let had_file_message = file_message.is_some();
    let template_message_source = file_message.is_none()
        && message_chunks.is_empty()
        && reuse_message.is_none()
        && fixup_commit.is_none()
        && squash_commit.is_none()
        && !amend
        && !in_merge
        && !in_cherry_pick
        && !in_revert
        && !git_dir.join("SQUASH_MSG").is_file()
        && template_file.is_some();
    let mut message = reused_commit
        .as_ref()
        .map(|commit| {
            if let Some(squash_message) = &squash_message {
                if squash_commit.as_deref() == reuse_message.as_deref() {
                    let mut message = b"squash! ".to_vec();
                    message.extend_from_slice(&commit.message);
                    message
                } else {
                    commit_squash_message(squash_message, Some(&commit.message), None, &[])
                }
            } else {
                commit.message.clone()
            }
        })
        .or_else(|| {
            squash_message.as_ref().map(|message| {
                commit_squash_message(message, None, file_message.as_deref(), &message_chunks)
            })
        })
        .or_else(|| {
            fixup_message.as_ref().map(|message| {
                commit_fixup_message(message, file_message.as_deref(), &message_chunks)
            })
        })
        .or_else(|| {
            if amend && file_message.is_none() && message_chunks.is_empty() {
                amended_commit.as_ref().map(|commit| commit.message.clone())
            } else {
                None
            }
        })
        .or_else(|| {
            if file_message.is_none()
                && message_chunks.is_empty()
                && reuse_message.is_none()
                && fixup_commit.is_none()
                && squash_commit.is_none()
                && !amend
                && git_dir.join("SQUASH_MSG").is_file()
            {
                read_squash_merge_message_from_file(&git_dir).ok()
            } else {
                None
            }
        })
        .or_else(|| {
            if (in_merge || in_cherry_pick || in_revert)
                && file_message.is_none()
                && message_chunks.is_empty()
                && reuse_message.is_none()
                && fixup_commit.is_none()
                && squash_commit.is_none()
            {
                if in_merge {
                    read_merge_message_from_file(&git_dir).ok()
                } else {
                    // Keep the commented "# Conflicts:" block intact: the
                    // editor template shows it and the post-editor cleanup
                    // strips it.
                    fs::read(git_dir.join("MERGE_MSG")).ok()
                }
            } else {
                None
            }
        })
        .or_else(|| {
            // `-t <file>`: the template body, used only when no `-m`/`-F`/`-C`
            // and not concluding a merge/cherry-pick. Read verbatim (git sets
            // `clean_message_contents = 0`).
            if file_message.is_none()
                && message_chunks.is_empty()
                && reuse_message.is_none()
                && fixup_commit.is_none()
                && squash_commit.is_none()
                && !amend
            {
                template_file
                    .as_deref()
                    .map(|path| read_commit_template_file(path))
                    .transpose()
                    .ok()
                    .flatten()
                    .flatten()
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            file_message.unwrap_or_else(|| commit_message_from_prepared_chunks(&message_chunks))
        });
    let all_index_snapshot = if all {
        Some(read_index_snapshot(&git_dir)?)
    } else {
        None
    };
    if all {
        commit_stage_tracked_changes(cli_session, &git_dir, format)?;
    }
    // Emptiness is judged before the signoff trailer is added (git aborts
    // `commit -m "" -s`).
    let empty_before_signoff = commit_message_is_empty(&commit_message_with_trailers(
        repo_config.as_ref(),
        &message,
        &trailers,
    ));
    // Editor flow: a commit without an explicit message source launches the
    // editor over COMMIT_EDITMSG (the in-merge / rebase conclude paths keep
    // their historical no-editor behavior).
    let had_message_source = had_file_message
        || !message_chunks.is_empty()
        || reuse_message.is_some()
        || fixup_commit.is_some()
        || squash_commit.is_some();
    let fixup_uses_editor = fixup_commit
        .as_ref()
        .is_some_and(CommitFixup::is_amend_style);
    let squash_uses_editor = squash_commit.is_some()
        && !had_file_message
        && message_chunks.is_empty()
        && reuse_message.is_none();
    let use_editor = !in_rebase
        && !in_merge
        && (edit_flag == Some(true)
            || (edit_flag != Some(false)
                && (!had_message_source
                    || reedit_message
                    || fixup_uses_editor
                    || squash_uses_editor)));
    let partial_head_tree = if !pathspec_args.is_empty() {
        let refs = FileRefStore::new(&git_dir, format);
        let head = commands::merge_rebase::head_commit_oid(&refs)?;
        let tree_map = match &head {
            Some(oid) => {
                let tree = commands::merge_rebase::commit_tree_oid(&commit_odb, format, oid)?;
                sley_diff_merge::flatten_tree(&commit_odb, format, &tree)?
            }
            None => BTreeMap::new(),
        };
        Some((head, tree_map))
    } else {
        None
    };
    let partial_index_snapshot = if partial_head_tree.is_some() {
        Some(read_index_snapshot(&git_dir)?)
    } else {
        None
    };
    if let Some((_, tree_map)) = &partial_head_tree
        && let Err(err) =
            stage_partial_commit_paths(cli_session, &git_dir, format, &pathspec_args, tree_map)
    {
        if let Some(snapshot) = &partial_index_snapshot {
            let _ = restore_index_snapshot(&git_dir, snapshot);
        }
        return Err(err);
    }
    let author_hook_env = commit_author_hook_env(&author)?;
    if !no_verify
        && let Err(err) = commands::hooks::run_hook_at(
            &git_dir,
            "pre-commit",
            commands::hooks::HookRun {
                env: author_hook_env.clone(),
                force_serial: true,
                ..commands::hooks::HookRun::default()
            },
        )
    {
        if let Some(snapshot) = &partial_index_snapshot {
            let _ = restore_index_snapshot(&git_dir, snapshot);
        }
        return Err(err);
    }
    // Resolve the cleanup mode now that `use_editor` is known. An explicit
    // `--cleanup`/`--no-cleanup` wins; otherwise `commit.cleanup` config; absent
    // both, git's editor-dependent default (ALL with an editor, SPACE without).
    let cleanup_config = cleanup_arg.clone().or_else(|| {
        read_repo_config(&git_dir)
            .ok()
            .and_then(|c| c.get("commit", None, "cleanup").map(str::to_string))
    });
    let cleanup_mode = resolve_commit_cleanup_mode(cleanup_config.as_deref(), use_editor);
    // git's prepare_to_commit: stripspace (when not verbatim) BEFORE signoff so
    // an empty `commit -s` keeps the two leading blank lines append_signoff
    // inserts for the title/body (t7502 "places sob on third line...").
    if cleanup_mode != CommitCleanupMode::Verbatim {
        message = commit_stripspace_message(&message, None);
    }
    let mut message = if signoff {
        commands::replay::append_signoff_before_comments(
            message,
            &commit_signoff_from_env(&identity_config)?,
        )
    } else {
        message
    };
    if !trailers.is_empty() {
        message =
            commit_message_with_trailers(repo_config.as_ref(), &message, &trailers).into_owned();
    }
    // core.commentChar=auto: pick an unused candidate from the user message
    // (git's adjust_comment_line_char), after stripspace+signoff and before the
    // status block is appended.
    let comment_char = resolve_commit_comment_char(&git_dir, &message)?;
    let editmsg = git_dir.join("COMMIT_EDITMSG");
    // When an editor will run, git appends a commented status block (the
    // template) to COMMIT_EDITMSG unless `--no-status`/`commit.status=false`.
    // `include_status` (cmdline) wins over `commit.status` config (default true).
    let include_status_resolved = include_status.unwrap_or_else(|| {
        read_repo_config(&git_dir)
            .ok()
            .and_then(|c| c.get_bool("commit", None, "status"))
            .unwrap_or(true)
    });
    // Message is already stripspace'd (unless verbatim) and signed off — write
    // it as-is. A second stripspace would eat the leading blanks append_signoff
    // placed for an empty buffer.
    let mut template = message.clone();
    if use_editor && include_status_resolved {
        // `author_date_is_interesting()` = `--date` given or author reused from
        // another commit (`-C`/`-c`/amend); env GIT_AUTHOR_DATE alone does not
        // trigger the template Date line.
        let author_date_interesting = author_date.is_some() || reuse_message.is_some() || amend;
        let block = build_commit_editor_template_block(&CommitTemplateBlock {
            git_dir: &git_dir,
            worktree_root: &worktree_root_for_git_dir(cli_session, &git_dir)?,
            format,
            comment_char: &comment_char,
            cleanup_mode,
            allow_empty_message,
            author: &author,
            committer: &committer,
            author_date_interesting,
            amend,
            untracked_override: commit_untracked,
        })?;
        template.extend_from_slice(&block);
    }
    if use_editor && verbose > 0 {
        append_commit_verbose_diff(
            cli_session,
            &git_dir,
            format,
            amend,
            verbose as u8,
            &comment_char,
            &mut template,
            cli_session.lazy_fetch(),
        )?;
    }
    fs::write(&editmsg, &template)?;
    let prepare_source = if amend {
        PrepareCommitMsgSource::Commit("HEAD")
    } else if in_rebase && (in_cherry_pick || in_revert) && git_dir.join("MERGE_MSG").is_file() {
        PrepareCommitMsgSource::Message
    } else if in_rebase && git_dir.join("MERGE_MSG").is_file() {
        PrepareCommitMsgSource::Merge
    } else if in_merge
        || ((in_cherry_pick || in_revert) && git_dir.join("MERGE_MSG").is_file())
        || (git_dir.join("SQUASH_MSG").is_file() && git_dir.join("MERGE_MSG").is_file())
    {
        PrepareCommitMsgSource::Merge
    } else if let Some(rev) = reuse_message.as_deref() {
        PrepareCommitMsgSource::Commit(rev)
    } else if had_message_source {
        PrepareCommitMsgSource::Message
    } else if template_message_source {
        PrepareCommitMsgSource::Template
    } else {
        PrepareCommitMsgSource::None
    };
    run_prepare_commit_msg_hook(
        &git_dir,
        &editmsg,
        prepare_source,
        author_hook_env.clone(),
        !use_editor && !in_rebase,
    )?;
    let editmsg_arg = editmsg.to_string_lossy().into_owned();
    let has_unmerged_index_entries = commit_index_has_unmerged_entries(&git_dir, format)?;
    if use_editor
        && !has_unmerged_index_entries
        && let Err(err) = commands::replay::launch_editor(&git_dir, &editmsg)
    {
        eprintln!("error: {err}");
        eprintln!("Please supply the message using either -m or -F option.");
        return Err(GitError::Exit(1));
    }
    if !no_verify {
        commands::hooks::run_hook_at(
            &git_dir,
            "commit-msg",
            commands::hooks::HookRun {
                args: vec![editmsg_arg.clone()],
                env: author_hook_env,
                force_serial: true,
                ..commands::hooks::HookRun::default()
            },
        )?;
    }
    message = fs::read(&editmsg)?;
    message = commit_cleanup_message(message, cleanup_mode, &comment_char, verbose > 0);
    if (in_cherry_pick || in_revert) && !allow_empty_message && commit_message_is_empty(&message) {
        let _ = restore_taken_index_snapshot(&git_dir, &all_index_snapshot);
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    // `git commit` invoked manually during a conflicted rebase concludes the
    // current rebase step (builtin: the sequencer's `do_commit` path). `--amend`
    // is the exception: git's `commit` builtin is rebase-agnostic — it amends
    // HEAD with the new tree/metadata and never re-derives a parent from the
    // rebase state, so a clean tree (date/author-only amend) must still proceed.
    // Without this guard a stale `rebase-merge/` directory wrongly routes
    // `commit --amend` through the rebase-step concluder, which fires its own
    // `tree == parent_tree -> "nothing to commit, working tree clean"` gate and
    // exits 1 (t3436 date tests: amend after a leftover rebase dir).
    if in_rebase && !amend {
        return conclude_rebase_step_via_commit(
            &git_dir,
            format,
            author,
            committer,
            message,
            quiet,
            allow_empty,
            cli_session.lazy_fetch(),
            cli_session.replace_objects(),
        );
    }
    if in_merge {
        return conclude_in_progress_merge(
            &git_dir,
            &worktree_root_for_git_dir(cli_session, &git_dir)?,
            format,
            message,
            quiet,
            &identity_config,
            cli_session.lazy_fetch(),
            cli_session.replace_objects(),
        );
    }
    if in_cherry_pick || in_revert {
        return conclude_replay_via_commit(
            &git_dir,
            &worktree_root_for_git_dir(cli_session, &git_dir)?,
            format,
            message,
            allow_empty,
            allow_empty_message,
            author,
            author_override.is_none() && !reset_author,
            quiet,
            &identity_config,
            cli_session.lazy_fetch(),
        );
    }
    if !allow_empty_message && empty_before_signoff && !use_editor {
        let _ = restore_taken_index_snapshot(&git_dir, &all_index_snapshot);
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    if !allow_empty_message
        && (commit_message_is_empty(&message)
            || (template_message_source
                && cleanup_mode_strips_comments(cleanup_mode)
                && commit_message_lacks_non_trailer_content(&message)))
    {
        let _ = restore_taken_index_snapshot(&git_dir, &all_index_snapshot);
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    if fixup_commit
        .as_ref()
        .is_some_and(CommitFixup::is_amend_style)
        && message.starts_with(b"amend! ")
        && !allow_empty_message
        && commit_message_is_empty(&commit_message_body(&message))
    {
        let _ = restore_taken_index_snapshot(&git_dir, &all_index_snapshot);
        eprintln!("Aborting commit due to empty commit message body.");
        return Err(GitError::Exit(1));
    }
    if commit_message_has_nul(&message) {
        let _ = restore_taken_index_snapshot(&git_dir, &all_index_snapshot);
        eprintln!("error: a NUL byte in commit log message not allowed.");
        return Err(GitError::Exit(1));
    }
    if encoding_is_utf8(&commit_encoding) && commit_message_has_invalid_utf8(&message) {
        eprintln!("Warning: commit message did not conform to UTF-8.");
    }
    if let Some(path) = template_file.as_deref()
        && !allow_empty_message
        && template_message_source
        && cleanup_mode_strips_comments(cleanup_mode)
        && let Some(template_bytes) = read_commit_template_file(path)?
        && commit_template_lacks_edit_content(
            &message,
            &template_bytes,
            cleanup_mode,
            &comment_char,
        )
    {
        let _ = restore_taken_index_snapshot(&git_dir, &all_index_snapshot);
        eprintln!("Aborting commit; you did not edit the message.");
        return Err(GitError::Exit(1));
    }
    if !pathspec_args.is_empty() {
        let (head, tree_map) = partial_head_tree.expect("partial commit precomputed HEAD tree");
        let parents = if amend {
            amended_commit
                .as_ref()
                .map(|commit| commit.parents.clone())
                .unwrap_or_default()
        } else {
            head.iter().copied().collect()
        };
        return commit_partial_paths(
            cli_session,
            &git_dir,
            format,
            &pathspec_args,
            head,
            parents,
            tree_map,
            author,
            committer,
            message,
            commit_encoding_header,
            quiet,
            amend,
            no_post_rewrite,
        );
    }
    let precomputed_index_tree = if !allow_empty && !amend && fixup_reword_tree.is_none() {
        match commit_index_tree_if_changed(&git_dir, format, &commit_odb)? {
            Some(tree) => Some(tree),
            None => {
                print_clean_commit_status(cli_session, &git_dir, format)?;
                return Err(GitError::Exit(1));
            }
        }
    } else {
        None
    };
    // Retain copies for the post-commit summary (the options struct moves them).
    let summary = if quiet {
        None
    } else {
        Some((author.clone(), committer.clone(), message.clone()))
    };
    let signature = if gpg_sign {
        let signed_tree = if let Some(tree) = fixup_reword_tree.as_ref() {
            *tree
        } else if let Some(tree) = precomputed_index_tree.as_ref() {
            *tree
        } else {
            sley_worktree::write_tree_from_index(&git_dir, format)?
        };
        let signed_parents = if amend {
            amended_commit
                .as_ref()
                .map(|commit| commit.parents.clone())
                .unwrap_or_default()
        } else {
            head_commit_oid(&FileRefStore::new(&git_dir, format))?
                .into_iter()
                .collect()
        };
        let unsigned = Commit {
            tree: signed_tree,
            parents: signed_parents,
            author: author.clone(),
            committer: committer.clone(),
            encoding: commit_encoding_header.clone(),
            message: message.clone(),
        };
        let key = commands::signing::signing_key(
            repo_config.as_ref(),
            gpg_sign_key.as_deref(),
            &committer,
        );
        Some(commands::signing::sign_payload(
            repo_config.as_ref(),
            &unsigned.write(),
            key.as_deref(),
        )?)
    } else {
        None
    };
    let initial_commit = !amend
        && commands::merge_rebase::head_commit_oid(&FileRefStore::new(&git_dir, format))?.is_none();
    let options = sley_sequencer::CommitIndexOptions {
        author,
        committer,
        reflog_message: commit_reflog_message_with_initial(&message, amend, initial_commit),
        message,
        encoding: commit_encoding_header,
        signature,
    };
    let result = if amend {
        sley_sequencer::amend_index(&git_dir, format, options)
    } else if let Some(tree) = fixup_reword_tree {
        sley_sequencer::commit_tree_at_head(&git_dir, format, tree, options)
    } else if let Some(tree) = precomputed_index_tree {
        sley_sequencer::commit_tree_at_head_with_odb(&git_dir, format, tree, options, &commit_odb)
    } else {
        sley_sequencer::commit_index(&git_dir, format, options)
    }?;
    commands::rerere::record_resolved_after_commit(
        &git_dir,
        &worktree_root_for_git_dir(cli_session, &git_dir)?,
        format,
    )?;
    remove_commit_state_files(
        &git_dir,
        &worktree_root_for_git_dir(cli_session, &git_dir)?,
        cli_session.lazy_fetch(),
    );
    commands::hooks::run_post_index_change_hook_at(&git_dir, false, false)?;
    if let Some((summary_author, summary_committer, summary_message)) = summary {
        print_commit_summary(
            &git_dir,
            format,
            &commit_odb,
            &result.oid,
            result.parent.as_ref(),
            &summary_message,
            &summary_author,
            &summary_committer,
            cli_session.lazy_fetch(),
        )?;
    }
    commands::hooks::run_hook_at(
        &git_dir,
        "reference-transaction",
        commands::hooks::HookRun::default(),
    )?;
    commands::hooks::run_hook_at(&git_dir, "post-commit", commands::hooks::HookRun::default())?;
    run_auto_maintenance_after_commit(cli_session, &git_dir)?;
    if amend
        && !no_post_rewrite
        && let Some(old_oid) = amended_old_oid
    {
        commands::hooks::run_hook_at(
            &git_dir,
            "post-rewrite",
            commands::hooks::HookRun {
                args: vec!["amend".to_string()],
                stdin: Some(format!("{} {}\n", old_oid, result.oid).into_bytes()),
                ..commands::hooks::HookRun::default()
            },
        )?;
    }
    Ok(())
}

fn run_auto_maintenance_after_commit(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
) -> Result<()> {
    commands::pack::trace2_touch();
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let config = read_repo_config(&common_git_dir)?;
    if !config.get_bool("maintenance", None, "auto").unwrap_or(true) {
        return Ok(());
    }
    let detach = config
        .get_bool("maintenance", None, "autoDetach")
        .or_else(|| config.get_bool("gc", None, "autoDetach"))
        .unwrap_or(true);
    let detach_arg = if detach { "--detach" } else { "--no-detach" };
    let trace_args = ["maintenance", "run", "--auto", "--quiet", detach_arg];
    commands::pack::trace2_child_start(&trace_args);
    let run_args = ["run", "--auto", "--quiet", detach_arg]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let _ = commands::pack::cmd_maintenance(cli_session, &run_args);
    Ok(())
}

/// Print git's post-commit summary (`print_commit_summary`), e.g.
/// `[main (root-commit) 0bed67f] initial` followed by an optional `Author:`/
/// `Committer:` line and the shortstat + `create/delete mode` summary of the diff
/// against the parent. `new_oid` is the freshly written commit; `parent` is its
/// first parent (None for a root commit, which diffs against the empty tree and
/// adds the `(root-commit)` marker). `author`/`committer` are the raw identity
/// buffers (`Name <email> seconds tz`); the `Author:` line is emitted only when
/// they differ in name/email, matching git. The object database is borrowed from
/// the caller so the summary reuses any hot pack/MIDX state from the commit path
/// instead of opening a second database for the same repository.
fn print_commit_summary(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    new_oid: &ObjectId,
    parent: Option<&ObjectId>,
    message: &[u8],
    author: &[u8],
    committer: &[u8],
    lazy_fetch: bool,
) -> Result<()> {
    // HEAD branch name, or "detached HEAD" / "HEAD" when unresolvable.
    let head = match repo_current_branch_name(git_dir) {
        Some(name) => name,
        None => "detached HEAD".to_string(),
    };
    let abbrev = commit_summary_abbrev(git_dir, format, new_oid)?;
    let root = if parent.is_none() {
        " (root-commit)"
    } else {
        ""
    };
    let subject = commit_subject(message);

    let mut out = io::stdout();
    write!(out, "[{head}{root} {abbrev}] {subject}\n")?;

    // `Author:` line when the author identity (name <email>) differs from the
    // committer's — git's `strbuf_cmp(&author_ident, &committer_ident)`.
    let author_id = identity_name_email(author);
    let committer_id = identity_name_email(committer);
    if author_id != committer_id {
        writeln!(out, " Author: {author_id}")?;
    }

    // Shortstat + summary of the diff against the parent tree (empty tree for a
    // root commit), matching `DIFF_FORMAT_SHORTSTAT | DIFF_FORMAT_SUMMARY`.
    let new_tree = read_commit_tree_for_summary(db, format, new_oid)?;
    let old_tree = match parent {
        Some(p) => read_commit_tree_for_summary(db, format, p)?,
        None => ObjectId::empty_tree(format),
    };
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        &old_tree,
        &new_tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    if !entries.is_empty() {
        let stat_entries = collect_diff_stat_entries(&entries, db, None, false, lazy_fetch)?;
        write_diff_shortstat_materialized(&mut out, &stat_entries)?;
        for entry in &entries {
            write_commit_summary_entry(&mut out, entry)?;
        }
    }
    out.flush()?;
    Ok(())
}

/// The post-commit summary uses the repository's effective abbreviation width.
/// Avoid per-commit uniqueness scans here: on large packed repositories that
/// turns a one-file commit into an all-object walk before the first line prints.
fn commit_summary_abbrev(git_dir: &Path, format: ObjectFormat, oid: &ObjectId) -> Result<String> {
    let hex = oid.to_hex();
    let width = repository_abbrev(git_dir, format)?
        .map(|width| width.min(hex.len()))
        .unwrap_or(hex.len());
    Ok(hex[..width].to_string())
}

/// Extract `Name <email>` from a raw git identity buffer (`Name <email> seconds
/// tz`) by trimming the trailing ` seconds timezone`. Used to compare author and
/// committer identities for the summary's `Author:` line.
fn identity_name_email(identity: &[u8]) -> String {
    let text = String::from_utf8_lossy(identity);
    match text.rfind('>') {
        Some(idx) => text[..=idx].to_string(),
        None => text.trim_end().to_string(),
    }
}

/// Read a commit's tree oid for the summary diff.
fn read_commit_tree_for_summary(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(commit_oid)?;
    let commit = Commit::parse(format, &object.body)?;
    Ok(commit.tree)
}

/// The ` create mode`/` delete mode`/` rename`/` copy`/` mode change` summary
/// line for one diff entry, matching git's `DIFF_FORMAT_SUMMARY`.
fn write_commit_summary_entry(
    out: &mut dyn Write,
    entry: &sley_diff_merge::NameStatusEntry,
) -> Result<()> {
    match entry.status {
        sley_diff_merge::NameStatus::Added => {
            let mode = entry.new_mode.unwrap_or(0);
            writeln!(out, " create mode {mode:06o} {}", entry.path)?;
        }
        sley_diff_merge::NameStatus::Deleted => {
            let mode = entry.old_mode.unwrap_or(0);
            writeln!(out, " delete mode {mode:06o} {}", entry.path)?;
        }
        sley_diff_merge::NameStatus::Renamed(score) => {
            if let Some(old_path) = &entry.old_path {
                writeln!(out, " rename {old_path} => {} ({score}%)", entry.path)?;
            }
        }
        sley_diff_merge::NameStatus::Copied(score) => {
            if let Some(old_path) = &entry.old_path {
                writeln!(out, " copy {old_path} => {} ({score}%)", entry.path)?;
            }
        }
        sley_diff_merge::NameStatus::Modified => {
            if let (Some(old_mode), Some(new_mode)) = (entry.old_mode, entry.new_mode)
                && old_mode != new_mode
            {
                writeln!(
                    out,
                    " mode change {old_mode:06o} => {new_mode:06o} {}",
                    entry.path
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Apply `commit --trailer` arguments to a (byte) commit message through the
/// full interpret-trailers engine (`commands::interpret_trailers`), so per-token
/// `trailer.*` config governs placement/policy/key/command exactly as `git commit
/// --trailer` does. A message with no queued trailers is returned untouched.
///
/// Commit messages are UTF-8 in practice; we losslessly round-trip via
/// `from_utf8_lossy` so non-UTF-8 bytes don't crash the (text-oriented) engine.
fn commit_message_with_trailers<'a>(
    config: Option<&GitConfig>,
    message: &'a [u8],
    trailers: &[String],
) -> std::borrow::Cow<'a, [u8]> {
    if trailers.is_empty() {
        return std::borrow::Cow::Borrowed(message);
    }
    let text = String::from_utf8_lossy(message);
    std::borrow::Cow::Owned(
        commands::interpret_trailers::apply_trailers_to_message(config, &text, trailers)
            .into_bytes(),
    )
}

/// Conclude an in-progress cherry-pick / revert via `git commit`: commit the
/// staged resolution with the picked commit's authorship, then run the
/// sequencer post-commit cleanup (CHERRY_PICK_HEAD / REVERT_HEAD removal and
/// the last-pick sequencer-state teardown).
#[allow(clippy::too_many_arguments)]
fn conclude_replay_via_commit(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    message: Vec<u8>,
    allow_empty: bool,
    allow_empty_message: bool,
    env_author: Vec<u8>,
    use_pick_author: bool,
    quiet: bool,
    config: &GitConfig,
    lazy_fetch: bool,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let refs = FileRefStore::new(git_dir, format);
    let head = commands::merge_rebase::head_commit_oid(&refs)?;
    let index_path = sley_worktree::repository_index_path(git_dir);
    if let Ok(bytes) = fs::read(&index_path) {
        let index = Index::parse(&bytes, format)?;
        let unmerged: BTreeSet<String> = index
            .entries
            .iter()
            .filter(|entry| index_entry_stage(entry) > 0)
            .map(|entry| entry.path.to_string())
            .collect();
        if !unmerged.is_empty() {
            for path in &unmerged {
                println!("U\t{path}");
            }
            eprintln!("error: Committing is not possible because you have unmerged files.");
            eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
            eprintln!("hint: as appropriate to mark resolution and make a commit.");
            eprintln!("fatal: Exiting because of an unresolved conflict.");
            return Err(GitError::Exit(128));
        }
    }
    let head_tree = match &head {
        Some(oid) => commands::merge_rebase::commit_tree_oid(&db, format, oid)?,
        None => ObjectId::empty_tree(format),
    };
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;
    let cherry_pick_head = git_dir.join("CHERRY_PICK_HEAD");
    if !allow_empty && tree == head_tree {
        let action = if cherry_pick_head.is_file() {
            "cherry-pick"
        } else {
            "revert"
        };
        eprintln!("The previous cherry-pick is now empty, possibly due to conflict resolution.");
        eprintln!("If you wish to commit it anyway, use:");
        eprintln!();
        eprintln!("    git commit --allow-empty");
        eprintln!();
        eprintln!("Otherwise, please use 'git {action} --skip'");
        return Err(GitError::Exit(1));
    }
    if !allow_empty_message && commit_message_is_empty(&message) {
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    let author = if use_pick_author && cherry_pick_head.is_file() {
        let text = fs::read_to_string(&cherry_pick_head)?;
        let oid = ObjectId::from_hex(format, text.trim())?;
        let object = db.read_object(&oid)?;
        Commit::parse(format, &object.body)?.author
    } else {
        env_author
    };
    let committer = commit_identity_from_env("COMMITTER", config)?;
    let new_oid = sley_sequencer::create_commit(
        &mut FileObjectDatabase::from_git_dir(git_dir, format),
        sley_sequencer::CommitCreate {
            tree,
            parents: head.iter().copied().collect(),
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding: None,
            signature: None,
        },
    )?;
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let old_oid = head.unwrap_or_else(|| ObjectId::null(format));
    let reflog = refs
        .should_write_reflog_for_update(&target_ref, false)?
        .then(|| ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: commit_reflog_message_with_initial(&message, false, head.is_none()),
        });
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: head.map(RefTarget::Direct),
        new: RefTarget::Direct(new_oid),
        reflog,
    });
    tx.commit()?;
    sley_sequencer::replay::post_commit_cleanup(git_dir);
    remove_commit_state_files(git_dir, worktree_root, lazy_fetch);
    commands::hooks::run_post_index_change_hook_at(git_dir, false, false)?;
    if !quiet {
        println!("{new_oid}");
    }
    commands::hooks::run_hook_at(git_dir, "post-commit", commands::hooks::HookRun::default())?;
    Ok(())
}

/// Partial commit (`git commit [-m ...] -- <paths>`): stage the named paths'
/// working-tree contents (clean filters applied, directories expanded over
/// the tracked entries beneath them), then record HEAD's tree with just those
/// paths replaced. Mirrors git's `--only` default for tracked-file usage.
fn commit_partial_paths(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[String],
    head: Option<ObjectId>,
    parents: Vec<ObjectId>,
    mut tree_map: BTreeMap<Vec<u8>, (u32, ObjectId)>,
    author: Vec<u8>,
    committer: Vec<u8>,
    message: Vec<u8>,
    encoding: Option<Vec<u8>>,
    quiet: bool,
    amend: bool,
    no_post_rewrite: bool,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let refs = FileRefStore::new(git_dir, format);
    let rel_paths = stage_partial_commit_paths(cli_session, git_dir, format, paths, &tree_map)?;
    let index_path = sley_worktree::repository_index_path(git_dir);
    // Overlay the staged state of the matched paths onto HEAD's tree.
    let updated_index = Index::parse(&fs::read(&index_path)?, format)?;
    let staged: BTreeMap<Vec<u8>, (u32, ObjectId)> = updated_index
        .entries
        .iter()
        .filter(|entry| index_entry_stage(entry) == 0)
        .map(|entry| (entry.path.clone().into_bytes(), (entry.mode, entry.oid)))
        .collect();
    for rel in &rel_paths {
        match staged.get(rel) {
            Some(entry) => {
                tree_map.insert(rel.clone(), *entry);
            }
            None => {
                tree_map.remove(rel);
            }
        }
    }
    let tree = write_tree_from_entry_map(&db, format, &tree_map)?;
    let new_oid = sley_sequencer::create_commit(
        &mut FileObjectDatabase::from_git_dir(git_dir, format),
        sley_sequencer::CommitCreate {
            tree,
            parents,
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding,
            signature: None,
        },
    )?;
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let old_oid = head.unwrap_or_else(|| ObjectId::null(format));
    let reflog = refs
        .should_write_reflog_for_update(&target_ref, false)?
        .then(|| ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: commit_reflog_message_with_initial(&message, amend, head.is_none()),
        });
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: head.map(RefTarget::Direct),
        new: RefTarget::Direct(new_oid),
        reflog,
    });
    tx.commit()?;
    sley_worktree::refresh_repository_cache_tree(git_dir, format, &db)?;
    sley_sequencer::replay::post_commit_cleanup(git_dir);
    remove_commit_state_files(
        git_dir,
        &worktree_root_for_git_dir(cli_session, git_dir)?,
        cli_session.lazy_fetch(),
    );
    if !quiet {
        println!("{new_oid}");
    }
    commands::hooks::run_hook_at(git_dir, "post-commit", commands::hooks::HookRun::default())?;
    if amend
        && !no_post_rewrite
        && let Some(old_oid) = head
    {
        commands::hooks::run_hook_at(
            git_dir,
            "post-rewrite",
            commands::hooks::HookRun {
                args: vec!["amend".to_string()],
                stdin: Some(format!("{old_oid} {new_oid}\n").into_bytes()),
                ..commands::hooks::HookRun::default()
            },
        )?;
    }
    Ok(())
}

fn stage_partial_commit_paths(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[String],
    head_tree_map: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
) -> Result<Vec<Vec<u8>>> {
    let worktree_root = worktree_root_for_git_dir(cli_session, git_dir)?;
    let cwd = env::current_dir()?;
    let index_path = sley_worktree::repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let known: BTreeSet<Vec<u8>> = index
        .entries
        .iter()
        .map(|entry| entry.path.clone().into_bytes())
        .chain(head_tree_map.keys().cloned())
        .collect();

    let root = fs::canonicalize(&worktree_root)?;
    let cwd = fs::canonicalize(&cwd)?;
    let prefix = cwd
        .strip_prefix(&root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/").into_bytes())
        .unwrap_or_default();
    let mut pathspecs = Vec::with_capacity(paths.len());
    let mut have_include = false;
    for path in paths {
        let parse_path = normalize_absolute_cli_pathspec(&root, &cwd, path)?;
        let element = sley_pathspec::parse_normalized_pathspec_element(
            &prefix,
            &parse_path,
            effective_pathspec_flags(cli_session),
        )?;
        have_include |= !element.is_exclude();
        pathspecs.push((path.as_str(), element, false));
    }

    // Expand the pathspecs over the tracked entries using git pathspec
    // semantics. With only negative pathspecs, commit's implicit include is the
    // whole tree, while the negative patterns themselves are still cwd-relative.
    let mut rel_paths: Vec<Vec<u8>> = Vec::new();
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    for tracked in &known {
        let mut included = false;
        let mut excluded = false;
        for (_, element, matched) in &mut pathspecs {
            if element.matches_path(tracked) {
                *matched = true;
                if element.is_exclude() {
                    excluded = true;
                } else {
                    included = true;
                }
            }
        }
        if !excluded && (!have_include || included) && seen.insert(tracked.clone()) {
            rel_paths.push(tracked.clone());
        }
    }
    for (path, element, matched) in &pathspecs {
        if !element.is_exclude() && !matched {
            eprintln!("error: pathspec '{path}' did not match any file(s) known to git");
            return Err(GitError::Exit(128));
        }
    }

    // Stage the matched paths with the regular add machinery (clean filters,
    // mode bits) — partial commits update those index entries too. When the
    // worktree executable bit is untrusted, keep the path's existing index/HEAD
    // regular-file mode; `commit <path>` should not turn an indexed 100755 file
    // into 100644 just because the filesystem reports it that way.
    let config = read_repo_config(git_dir)?;
    let mode_preferences = if config.get_bool("core", None, "fileMode").unwrap_or(true) {
        BTreeMap::new()
    } else {
        partial_commit_untrusted_mode_preferences(&index, head_tree_map, &rel_paths)
    };
    // Partial-commit staging applies one uniform mode (`--add --remove`) to
    // every matched path, so stamp that mode onto each `UpdateIndexPath`.
    let commit_mode = sley_worktree::UpdateIndexPathMode {
        add: true,
        remove: true,
        force_remove: false,
        info_only: false,
        chmod: None,
    };
    let ordered: Vec<sley_worktree::UpdateIndexPath> = rel_paths
        .iter()
        .map(|rel| sley_worktree::UpdateIndexPath {
            path: worktree_root.join(String::from_utf8_lossy(rel).as_ref()),
            mode: commit_mode,
        })
        .collect();
    sley_worktree::update_index_ordered_paths_filtered(
        &worktree_root,
        git_dir,
        format,
        &ordered,
        sley_worktree::UpdateIndexOptions {
            add: true,
            remove: true,
            force_remove: false,
            chmod: None,
            info_only: false,
            ignore_skip_worktree_entries: false,
            allow_skip_worktree_entries: false,
        },
        &config,
        false,
    )?;
    restore_partial_commit_untrusted_modes(git_dir, format, &mode_preferences)?;
    Ok(rel_paths)
}

fn partial_commit_untrusted_mode_preferences(
    index: &Index,
    head_tree_map: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
    rel_paths: &[Vec<u8>],
) -> BTreeMap<Vec<u8>, u32> {
    rel_paths
        .iter()
        .filter_map(|rel| {
            let mode = index
                .entries
                .iter()
                .find(|entry| index_entry_stage(entry) == 0 && entry.path.as_bytes() == rel)
                .map(|entry| entry.mode)
                .or_else(|| head_tree_map.get(rel).map(|(mode, _)| *mode))?;
            partial_commit_preservable_regular_mode(mode).then(|| (rel.clone(), mode))
        })
        .collect()
}

fn partial_commit_preservable_regular_mode(mode: u32) -> bool {
    matches!(mode, 0o100644 | 0o100755)
}

fn restore_partial_commit_untrusted_modes(
    git_dir: &Path,
    format: ObjectFormat,
    mode_preferences: &BTreeMap<Vec<u8>, u32>,
) -> Result<()> {
    if mode_preferences.is_empty() {
        return Ok(());
    }
    let index_path = sley_worktree::repository_index_path(git_dir);
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let mut changed = false;
    for entry in &mut index.entries {
        if index_entry_stage(entry) != 0 {
            continue;
        }
        let Some(mode) = mode_preferences.get(entry.path.as_bytes()) else {
            continue;
        };
        if partial_commit_preservable_regular_mode(entry.mode) && entry.mode != *mode {
            entry.mode = *mode;
            changed = true;
        }
    }
    if changed {
        fs::write(index_path, index.write(format)?)?;
    }
    Ok(())
}

fn read_index_snapshot(git_dir: &Path) -> Result<Option<Vec<u8>>> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    match fs::read(&index_path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn restore_index_snapshot(git_dir: &Path, snapshot: &Option<Vec<u8>>) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    match snapshot {
        Some(bytes) => fs::write(index_path, bytes)?,
        None => match fs::remove_file(index_path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        },
    }
    Ok(())
}

fn restore_taken_index_snapshot(git_dir: &Path, snapshot: &Option<Option<Vec<u8>>>) -> Result<()> {
    if let Some(snapshot) = snapshot {
        restore_index_snapshot(git_dir, snapshot)?;
    }
    Ok(())
}

fn remove_commit_state_files(git_dir: &Path, worktree_root: &Path, lazy_fetch: bool) {
    let format = repository_object_format(git_dir).ok();
    if let Some(format) = format {
        commands::merge_rebase::apply_merge_autostash(git_dir, worktree_root, format, lazy_fetch);
    }
    for name in [
        "MERGE_HEAD",
        "MERGE_MSG",
        "MERGE_MODE",
        "AUTO_MERGE",
        "SQUASH_MSG",
    ] {
        let _ = fs::remove_file(git_dir.join(name));
    }
}

/// Write a tree object hierarchy from a flat `path -> (mode, oid)` map
/// (grouping by leading path component, mirroring fast-import's writer).
fn write_tree_from_entry_map(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    entries: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
) -> Result<ObjectId> {
    let _ = format;
    write_entry_map_level(db, entries, &[])
}

fn write_entry_map_level(
    db: &FileObjectDatabase,
    entries: &BTreeMap<Vec<u8>, (u32, ObjectId)>,
    prefix: &[u8],
) -> Result<ObjectId> {
    let mut tree_entries: Vec<sley_object::TreeEntry> = Vec::new();
    let mut subdirs: BTreeSet<Vec<u8>> = BTreeSet::new();
    let prefix_len = if prefix.is_empty() {
        0
    } else {
        prefix.len() + 1
    };
    if prefix.is_empty() {
        for (path, (mode, oid)) in entries {
            add_entry_map_tree_item(
                &mut tree_entries,
                &mut subdirs,
                &path[prefix_len..],
                *mode,
                *oid,
            );
        }
    } else {
        for (path, (mode, oid)) in entries.range(prefix.to_vec()..) {
            if !path.starts_with(prefix) {
                break;
            }
            if path.get(prefix.len()) != Some(&b'/') {
                continue;
            }
            add_entry_map_tree_item(
                &mut tree_entries,
                &mut subdirs,
                &path[prefix_len..],
                *mode,
                *oid,
            );
        }
    }
    for dir in subdirs {
        let mut sub_prefix = prefix.to_vec();
        if !sub_prefix.is_empty() {
            sub_prefix.push(b'/');
        }
        sub_prefix.extend_from_slice(&dir);
        let sub_oid = write_entry_map_level(db, entries, &sub_prefix)?;
        tree_entries.push(sley_object::TreeEntry {
            mode: 0o040000,
            name: BString::from(dir),
            oid: sub_oid,
        });
    }
    // Tree entries collate with subtrees as though their name ends in `/`.
    tree_entries.sort_by_key(|entry| {
        let mut key = entry.name.clone().into_bytes();
        if entry.mode == 0o040000 {
            key.push(b'/');
        }
        key
    });
    db.write_object(EncodedObject::new(
        ObjectType::Tree,
        sley_object::Tree {
            entries: tree_entries,
        }
        .write(),
    ))
}

fn add_entry_map_tree_item(
    tree_entries: &mut Vec<sley_object::TreeEntry>,
    subdirs: &mut BTreeSet<Vec<u8>>,
    rel: &[u8],
    mode: u32,
    oid: ObjectId,
) {
    if let Some(slash) = rel.iter().position(|b| *b == b'/') {
        subdirs.insert(rel[..slash].to_vec());
    } else {
        tree_entries.push(sley_object::TreeEntry {
            mode,
            name: BString::from(rel.to_vec()),
            oid,
        });
    }
}

enum CommitFixup {
    Plain(String),
    Amend { rev: String, reword: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommitStatusMode {
    Normal,
    Short,
    Porcelain,
    Long,
}

fn cmd_commit_status_preview(
    cli_session: &crate::session::CliSession,
    mode: CommitStatusMode,
    null: bool,
    amend: bool,
    untracked: Option<sley_worktree::StatusUntrackedMode>,
) -> Result<()> {
    let mut args = Vec::new();
    match mode {
        CommitStatusMode::Normal => {}
        CommitStatusMode::Short => args.push("--short".to_string()),
        CommitStatusMode::Porcelain => args.push("--porcelain".to_string()),
        CommitStatusMode::Long => {
            return cmd_commit_long_status_preview(cli_session, amend, untracked);
        }
    }
    if null {
        args.push("-z".to_string());
    }
    if let Some(mode) = untracked {
        args.push(match mode {
            sley_worktree::StatusUntrackedMode::None => "--untracked-files=no".to_string(),
            sley_worktree::StatusUntrackedMode::Normal => "--untracked-files=normal".to_string(),
            sley_worktree::StatusUntrackedMode::All => "--untracked-files=all".to_string(),
        });
    }
    cmd_status(cli_session, &args)
}

fn cmd_commit_long_status_preview(
    cli_session: &crate::session::CliSession,
    amend: bool,
    untracked_override: Option<sley_worktree::StatusUntrackedMode>,
) -> Result<()> {
    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let config = read_repo_config(&git_dir).map_err(report_config_setup_error)?;
    let worktree_root = worktree_root_for_git_dir(cli_session, &git_dir)?;
    let format = repository_object_format(&git_dir)?;
    // `commit -u<mode>` wins over `status.showUntrackedFiles`; otherwise config
    // (then the normal default) applies.
    let untracked_mode = untracked_override.unwrap_or_else(|| {
        match config.get("status", None, "showUntrackedFiles") {
            Some("no") | Some("false") | Some("0") | Some("off") => {
                sley_worktree::StatusUntrackedMode::None
            }
            Some("all") => sley_worktree::StatusUntrackedMode::All,
            _ => sley_worktree::StatusUntrackedMode::Normal,
        }
    });
    let mut entries = crate::collect_short_status_with_options(
        &worktree_root,
        &git_dir,
        format,
        sley_worktree::ShortStatusOptions {
            include_ignored: false,
            ignored_mode: sley_worktree::StatusIgnoredMode::Traditional,
            untracked_mode,
            reference: if amend { Some("HEAD^1") } else { None },
        },
    )?;
    let committable = status_entries_have_index_changes(&entries);
    // `commit --dry-run` carries no `--ignore-submodules` flag, so the resolver
    // reflects only config; apply it so submodule worktree detail honours
    // `submodule.<name>.ignore` / `diff.ignoreSubmodules` the same as `status`.
    let ignore_resolver = SubmoduleIgnoreResolver::load(&git_dir, &config, None)?;
    apply_submodule_ignore(&mut entries, &ignore_resolver);
    // The staged summary compares against HEAD (or HEAD^ when amending, since the
    // amend commit replaces HEAD) — wt-status.c passes `s->amend ? "HEAD^" :
    // "HEAD"` to `git submodule summary --cached`.
    let base_ref = if amend { "HEAD^" } else { "HEAD" };
    let submodule_summary = status_submodule_summary(
        &git_dir,
        &worktree_root,
        format,
        &config,
        base_ref,
        &ignore_resolver,
    )?;
    let display = StatusLongDisplay {
        commit_preview: true,
        show_stash: false,
        ahead_behind: true,
        hints: config
            .get_bool("advice", None, "statusHints")
            .unwrap_or(true),
        untracked_suppressed: untracked_mode == sley_worktree::StatusUntrackedMode::None,
        comment_prefix: status_comment_prefix(&config),
        submodule_summary,
        sparse_footer: None,
        // `commit --dry-run` has no `-M`/`--no-renames`; rename detection comes
        // from `status.renames`/`diff.renames` config alone.
        rename_config: resolve_status_rename_config(&config, None, None),
    };
    print_status_long(&worktree_root, &git_dir, format, entries, &display)?;
    if committable {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

impl CommitFixup {
    fn parse(value: &str) -> Result<Self> {
        if let Some(rev) = value.strip_prefix("amend:") {
            Ok(Self::Amend {
                rev: rev.to_string(),
                reword: false,
            })
        } else if let Some(rev) = value.strip_prefix("reword:") {
            Ok(Self::Amend {
                rev: rev.to_string(),
                reword: true,
            })
        } else if value.contains(':')
            && value
                .split_once(':')
                .is_some_and(|(mode, _)| !mode.is_empty())
        {
            eprintln!("fatal: unknown option: --fixup={value}");
            Err(GitError::Exit(128))
        } else {
            Ok(Self::Plain(value.to_string()))
        }
    }

    fn rev(&self) -> &str {
        match self {
            Self::Plain(rev) | Self::Amend { rev, .. } => rev,
        }
    }

    fn is_amend_style(&self) -> bool {
        matches!(self, Self::Amend { .. })
    }

    fn is_reword(&self) -> bool {
        matches!(self, Self::Amend { reword: true, .. })
    }
}

/// `git commit -h`: print a usage synopsis and exit 129, matching upstream's
/// `parse-options`-driven `-h` handling (which fires before any repository
/// state is read, so it works even in a broken repo). The test only asserts
/// exit code 129 and a "[Uu]sage" match in the output.
fn commit_usage() -> Result<()> {
    eprintln!("usage: git commit [-a | --interactive | --patch] [-s] [-v] [-u<mode>] [--amend]");
    eprintln!(
        "                  [--dry-run] [(-c | -C | --squash) <commit> | --fixup [(amend|reword):]<commit>]"
    );
    eprintln!("                  [-F <file> | -m <msg>] [--reset-author] [--allow-empty]");
    eprintln!("                  [--no-verify] [-e] [--author=<author>] [--date=<date>]");
    eprintln!("                  [--cleanup=<mode>] [--[no-]status] [-i | -o] [pathspec...]");
    Err(GitError::Exit(129))
}

fn commit_author_requires_value_error() -> Result<()> {
    eprintln!("error: option `author' requires a value");
    Err(GitError::Exit(129))
}

fn commit_date_requires_value_error() -> Result<()> {
    eprintln!("error: option `date' requires a value");
    Err(GitError::Exit(129))
}

fn commit_cleanup_requires_value_error() -> Result<()> {
    eprintln!("error: option `cleanup' requires a value");
    Err(GitError::Exit(129))
}

fn commit_template_requires_value_error() -> Result<()> {
    eprintln!("error: option `template' requires a value");
    Err(GitError::Exit(129))
}

fn commit_template_short_requires_value_error() -> Result<()> {
    eprintln!("error: switch `t' requires a value");
    Err(GitError::Exit(129))
}

fn commit_reuse_message_requires_value_error(short: bool, reedit: bool) -> Result<()> {
    if short {
        let switch = if reedit { "c" } else { "C" };
        eprintln!("error: switch `{switch}' requires a value");
    } else {
        let option = if reedit {
            "reedit-message"
        } else {
            "reuse-message"
        };
        eprintln!("error: option `{option}' requires a value");
    }
    Err(GitError::Exit(129))
}

fn commit_fixup_requires_value_error() -> Result<()> {
    eprintln!("error: option `fixup' requires a value");
    Err(GitError::Exit(129))
}

fn commit_squash_requires_value_error() -> Result<()> {
    eprintln!("error: option `squash' requires a value");
    Err(GitError::Exit(129))
}

fn commit_trailer_requires_value_error() -> Result<()> {
    eprintln!("error: option `trailer' requires a value");
    Err(GitError::Exit(129))
}

fn commit_pathspec_from_file_requires_value_error() -> Result<()> {
    eprintln!("error: option `pathspec-from-file' requires a value");
    Err(GitError::Exit(129))
}

fn commit_pathspec_from_file_with_inline_pathspec_error() -> Result<()> {
    eprintln!("fatal: '--pathspec-from-file' and pathspec arguments cannot be used together");
    Err(GitError::Exit(128))
}

fn commit_option_takes_no_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

fn commit_invalid_untracked_files_mode_error(mode: &str) -> Result<()> {
    eprintln!("fatal: Invalid untracked files mode '{mode}'");
    Err(GitError::Exit(128))
}

fn commit_message_arg_chunk(message: &str) -> Vec<u8> {
    let mut chunk = argv_bytes_from_string(message);
    if !chunk.ends_with(b"\n") {
        chunk.push(b'\n');
    }
    chunk
}

fn read_porcelain_commit_message_file(path: &str) -> Result<Vec<u8>> {
    let mut message = read_commit_message_file(path)?;
    if !message.is_empty() && !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    Ok(message)
}

fn commit_message_is_empty(message: &[u8]) -> bool {
    message.iter().all(u8::is_ascii_whitespace)
}

fn commit_message_lacks_non_trailer_content(message: &[u8]) -> bool {
    !message_has_non_trailer_content(message, "#")
}

fn cleanup_mode_strips_comments(mode: CommitCleanupMode) -> bool {
    matches!(mode, CommitCleanupMode::Strip | CommitCleanupMode::Scissors)
}

fn message_has_non_trailer_content(message: &[u8], comment_char: &str) -> bool {
    let text = String::from_utf8_lossy(message);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(comment_char) {
            continue;
        }
        if !is_commit_trailer_line(line) {
            return true;
        }
    }
    false
}

fn is_commit_trailer_line(line: &str) -> bool {
    let Some((key, value)) = line.split_once(':') else {
        return false;
    };
    !key.is_empty()
        && !value.trim().is_empty()
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Validate a `--cleanup`/`commit.cleanup` mode string. git's `get_cleanup_mode`
/// `die`s on an unknown value (exit 128); the concrete mode is resolved later by
/// [`resolve_commit_cleanup_mode`] once `use_editor` is known.
fn validate_commit_cleanup_mode(value: &str) -> Result<()> {
    match value {
        "strip" | "whitespace" | "scissors" | "default" | "verbatim" => Ok(()),
        _ => {
            eprintln!("fatal: Invalid cleanup mode {value}");
            Err(GitError::Exit(128))
        }
    }
}

fn read_fixup_commit_message(
    git_dir: &Path,
    format: ObjectFormat,
    fixup: &CommitFixup,
    output_encoding: &str,
    replace_objects: bool,
) -> Result<Vec<u8>> {
    let commit = read_reused_commit(git_dir, format, fixup.rev(), replace_objects)?;
    let message = commit_message_for_commit_encoding(&commit, output_encoding);
    let subject = commit_subject_bytes(&message);
    match fixup {
        CommitFixup::Plain(_) => {
            let mut message = b"fixup! ".to_vec();
            message.extend_from_slice(&subject);
            message.push(b'\n');
            Ok(message)
        }
        CommitFixup::Amend { .. } => {
            let mut amend = b"amend! ".to_vec();
            amend.extend_from_slice(&subject);
            amend.extend_from_slice(b"\n\n");
            let body = if commit.message.starts_with(b"amend! ") {
                commit_message_body(&commit.message)
            } else {
                commit_message_for_commit_encoding(&commit, output_encoding).into_owned()
            };
            amend.extend_from_slice(&body);
            Ok(amend)
        }
    }
}

fn read_squash_commit_message(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
    output_encoding: &str,
    replace_objects: bool,
) -> Result<Vec<u8>> {
    let commit = read_reused_commit(git_dir, format, rev, replace_objects)?;
    let message = commit_message_for_commit_encoding(&commit, output_encoding);
    let subject = commit_subject_bytes(&message);
    let mut squash = b"squash! ".to_vec();
    squash.extend_from_slice(&subject);
    squash.push(b'\n');
    Ok(squash)
}

fn read_squash_merge_message_from_file(git_dir: &Path) -> Result<Vec<u8>> {
    let mut message = fs::read(git_dir.join("SQUASH_MSG"))?;
    if let Ok(merge_msg) = fs::read(git_dir.join("MERGE_MSG"))
        && let Some(conflicts) = merge_msg_conflicts_block(&merge_msg)
    {
        if !merge_msg_block_has_scissors(conflicts) && commit_cleanup_config_is_scissors(git_dir) {
            let comment_char = commit_comment_string(git_dir);
            message.push(b'\n');
            append_scissors_cut_line(&mut message, &comment_char);
            message.extend_from_slice(comment_char.as_bytes());
            message.push(b'\n');
            message.extend_from_slice(conflicts.strip_prefix(b"\n").unwrap_or(conflicts));
        } else {
            message.extend_from_slice(conflicts);
        }
    }
    Ok(message)
}

fn merge_msg_conflicts_block(message: &[u8]) -> Option<&[u8]> {
    let conflicts = message_line_start(message, b"# Conflicts:\n")?;
    let cut = message_line_start(
        message,
        b"# ------------------------ >8 ------------------------\n",
    )
    .filter(|cut| *cut < conflicts)
    .unwrap_or(conflicts);
    let start = if cut > 0 && message[cut - 1] == b'\n' {
        cut - 1
    } else {
        cut
    };
    Some(&message[start..])
}

fn message_line_start(message: &[u8], marker: &[u8]) -> Option<usize> {
    if message.starts_with(marker) {
        return Some(0);
    }
    message
        .windows(marker.len() + 1)
        .position(|window| window[0] == b'\n' && &window[1..] == marker)
        .map(|index| index + 1)
}

fn merge_msg_block_has_scissors(message: &[u8]) -> bool {
    message_line_start(
        message,
        b"# ------------------------ >8 ------------------------\n",
    )
    .is_some()
}

fn commit_cleanup_config_is_scissors(git_dir: &Path) -> bool {
    read_repo_config(git_dir)
        .ok()
        .and_then(|config| {
            config
                .get("commit", None, "cleanup")
                .map(|value| value == "scissors")
        })
        .unwrap_or(false)
}

fn commit_fixup_message(
    fixup_message: &[u8],
    file_message: Option<&[u8]>,
    message_chunks: &[Vec<u8>],
) -> Vec<u8> {
    let body = file_message
        .map(<[u8]>::to_vec)
        .unwrap_or_else(|| commit_message_from_prepared_chunks(message_chunks));
    if body.is_empty() {
        return fixup_message.to_vec();
    }
    let mut message = fixup_message.to_vec();
    if !message.ends_with(b"\n\n") {
        message.push(b'\n');
    }
    message.extend_from_slice(&body);
    message
}

fn commit_squash_message(
    squash_message: &[u8],
    reused_message: Option<&[u8]>,
    file_message: Option<&[u8]>,
    message_chunks: &[Vec<u8>],
) -> Vec<u8> {
    let body = reused_message
        .map(<[u8]>::to_vec)
        .or_else(|| file_message.map(<[u8]>::to_vec))
        .unwrap_or_else(|| commit_message_from_prepared_chunks(message_chunks));
    if body.is_empty() {
        return squash_message.to_vec();
    }
    let mut message = squash_message.to_vec();
    if !message.ends_with(b"\n\n") {
        message.push(b'\n');
    }
    message.extend_from_slice(&body);
    message
}

fn commit_message_body(message: &[u8]) -> Vec<u8> {
    let Some(first_lf) = message.iter().position(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    let body_start = if message.get(first_lf + 1) == Some(&b'\n') {
        first_lf + 2
    } else {
        first_lf + 1
    };
    message[body_start..].to_vec()
}

fn read_amended_commit(git_dir: &Path, format: ObjectFormat) -> Result<Commit> {
    match read_head_commit(git_dir, format)? {
        Some(commit) => Ok(commit),
        None => {
            eprintln!("fatal: You have nothing to amend.");
            Err(GitError::Exit(128))
        }
    }
}

fn read_head_commit(git_dir: &Path, format: ObjectFormat) -> Result<Option<Commit>> {
    let store = FileRefStore::new(git_dir, format);
    let head = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => store.read_ref(&name)?,
        direct => direct,
    };
    let Some(RefTarget::Direct(oid)) = head else {
        return Ok(None);
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            oid,
            object.object_type.as_str()
        )));
    }
    Commit::parse(format, &object.body).map(Some)
}

/// First line of the commit message at `oid`, for the `HEAD is now at <oid>
/// <subject>` line a detached-HEAD checkout prints. Best-effort: an unreadable or
/// non-commit object yields an empty subject (git still prints the abbreviated
/// oid).
fn build_reused_commit_author_identity(
    reused_author: &[u8],
    author: Option<&str>,
    date: Option<&str>,
) -> Result<Vec<u8>> {
    if author.is_none() && date.is_none() {
        validate_reused_commit_author_identity(reused_author)?;
        return Ok(reused_author.to_vec());
    }
    let (reused_name, reused_email, reused_date) =
        parse_commit_identity_parts_bytes(reused_author)?;
    let (name, email): (Vec<u8>, Vec<u8>) = if let Some(author) = author {
        parse_commit_author_bytes(&argv_bytes_from_string(author))?
    } else {
        (reused_name, reused_email)
    };
    // A `--date` override is raw user input; canonicalize it. The reused date
    // is already in canonical `<seconds> <tz>` form.
    let date = match date {
        Some(date) => canonicalize_commit_date(date),
        None => reused_date,
    };
    sley_sequencer::format_commit_identity_bytes(&name, &email, &date)
}

fn validate_reused_commit_author_identity(identity: &[u8]) -> Result<()> {
    if parse_commit_identity_parts(identity).is_ok() {
        return Ok(());
    }
    eprintln!("fatal: empty ident name (for <>) not allowed");
    Err(GitError::Exit(128))
}

fn commit_author_hook_env(author: &[u8]) -> Result<Vec<(String, String)>> {
    let (name, email, date) = parse_commit_identity_parts_bytes(author)?;
    Ok(vec![
        (
            "GIT_AUTHOR_NAME".to_string(),
            String::from_utf8_lossy(&name).into_owned(),
        ),
        (
            "GIT_AUTHOR_EMAIL".to_string(),
            String::from_utf8_lossy(&email).into_owned(),
        ),
        ("GIT_AUTHOR_DATE".to_string(), date),
    ])
}

fn parse_commit_identity_parts(identity: &[u8]) -> Result<(String, String, String)> {
    let identity = std::str::from_utf8(identity)
        .map_err(|err| GitError::InvalidObject(format!("invalid commit identity: {err}")))?;
    let Some((left, timezone)) = identity.rsplit_once(' ') else {
        return Err(GitError::InvalidObject(
            "commit identity missing timezone".into(),
        ));
    };
    let Some((author, timestamp)) = left.rsplit_once(' ') else {
        return Err(GitError::InvalidObject(
            "commit identity missing timestamp".into(),
        ));
    };
    let (name, email) = parse_commit_author(author)?;
    Ok((name, email, format!("{timestamp} {timezone}")))
}

fn parse_commit_identity_parts_bytes(identity: &[u8]) -> Result<(Vec<u8>, Vec<u8>, String)> {
    let Some(timezone_start) = identity.iter().rposition(|byte| *byte == b' ') else {
        return Err(GitError::InvalidObject(
            "commit identity missing timezone".into(),
        ));
    };
    let (left, timezone_with_space) = identity.split_at(timezone_start);
    let timezone = &timezone_with_space[1..];
    let Some(timestamp_start) = left.iter().rposition(|byte| *byte == b' ') else {
        return Err(GitError::InvalidObject(
            "commit identity missing timestamp".into(),
        ));
    };
    let (author, timestamp_with_space) = left.split_at(timestamp_start);
    let timestamp = &timestamp_with_space[1..];
    let (name, email) = parse_commit_author_bytes(author)?;
    let timestamp = std::str::from_utf8(timestamp)
        .map_err(|err| GitError::InvalidObject(format!("invalid commit timestamp: {err}")))?;
    let timezone = std::str::from_utf8(timezone)
        .map_err(|err| GitError::InvalidObject(format!("invalid commit timezone: {err}")))?;
    Ok((name, email, format!("{timestamp} {timezone}")))
}

fn commit_stage_tracked_changes(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let cwd = cli_session.cwd().to_path_buf();
    let worktree_root = worktree_root_for_git_dir(cli_session, git_dir)?;
    let actions = resolve_add_update_actions(
        &cwd,
        &worktree_root,
        git_dir,
        format,
        Vec::new(),
        false,
        false,
    )?;
    let mut seen_paths = BTreeSet::new();
    let mut action_paths = Vec::new();
    for path in actions.iter().map(AddAction::path) {
        if seen_paths.insert(path.clone()) {
            action_paths.push(path.clone());
        }
    }
    if let Some(index) = sley_worktree::read_repository_index(git_dir, format)? {
        for path in index
            .entries
            .iter()
            .filter(|entry| index_entry_stage(entry) > 0 || entry.is_intent_to_add())
            .map(|entry| worktree_root.join(repo_path_to_path(entry.path.as_bytes())))
        {
            if seen_paths.insert(path.clone()) {
                action_paths.push(path);
            }
        }
    }
    if action_paths.is_empty() {
        return Ok(());
    }
    let config = read_repo_config(git_dir)?;
    sley_worktree::update_index_paths_filtered(
        &worktree_root,
        git_dir,
        format,
        &action_paths,
        sley_worktree::UpdateIndexOptions {
            add: true,
            remove: true,
            force_remove: false,
            chmod: None,
            info_only: false,
            ignore_skip_worktree_entries: false,
            allow_skip_worktree_entries: false,
        },
        &config,
    )?;
    Ok(())
}

fn commit_index_has_unmerged_entries(git_dir: &Path, format: ObjectFormat) -> Result<bool> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    let Ok(bytes) = fs::read(&index_path) else {
        return Ok(false);
    };
    let index = Index::parse(&bytes, format)?;
    Ok(index
        .entries
        .iter()
        .any(|entry| index_entry_stage(entry) > 0))
}

fn commit_index_tree_if_changed(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<Option<ObjectId>> {
    let tree = sley_worktree::write_tree_from_index_with_odb(git_dir, format, db)?;
    let store = FileRefStore::new(git_dir, format);
    let head = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => store.read_ref(&name)?,
        direct => direct,
    };
    let Some(RefTarget::Direct(parent)) = head else {
        return Ok((tree != ObjectId::empty_tree(format)).then_some(tree));
    };
    let object = db.read_object(&parent)?;
    if object.object_type != ObjectType::Commit {
        return Ok(Some(tree));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    Ok((commit.tree != tree).then_some(tree))
}

/// Read a `-t <file>` / `--template <file>` template body. The path is relative
/// to the current working directory (git resolves it via the prefix). git reads
/// it verbatim (no whitespace cleanup).
fn read_commit_template_file(path: &str) -> Result<Option<Vec<u8>>> {
    let (optional, path) = match path.strip_prefix(":(optional)") {
        Some(path) => (true, path),
        None => (false, path),
    };
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if optional && err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => {
            eprintln!("fatal: could not read '{path}': {err}");
            Err(GitError::Exit(128))
        }
    }
}

fn commit_verbose_config(value: Option<&str>) -> Option<i32> {
    let value = value?;
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(1),
        "false" | "no" | "off" => Some(0),
        value => value.parse::<i32>().ok().map(|v| v.max(0)),
    }
}

fn commit_template_lacks_edit_content(
    message: &[u8],
    template: &[u8],
    cleanup_mode: CommitCleanupMode,
    comment_char: &str,
) -> bool {
    let cleaned_template =
        commit_cleanup_message(template.to_vec(), cleanup_mode, comment_char, false);
    if cleaned_template == message {
        return true;
    }
    let Some(extra) = message.strip_prefix(cleaned_template.as_slice()) else {
        return false;
    };
    !message_has_non_trailer_content(extra, comment_char)
}

fn append_commit_verbose_diff(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    format: ObjectFormat,
    amend: bool,
    verbose: u8,
    comment_char: &str,
    out: &mut Vec<u8>,
    lazy_fetch: bool,
) -> Result<()> {
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    if !message_has_scissors_cut_line(out, comment_char) {
        append_scissors_cut_line(out, comment_char);
    }
    if verbose == 1 {
        append_commit_diff_index_patch(
            cli_session,
            git_dir,
            format,
            amend,
            "a/",
            "b/",
            false,
            out,
            lazy_fetch,
        )?;
    } else {
        out.extend_from_slice(b"Changes to be committed:\n");
        append_commit_diff_index_patch(
            cli_session,
            git_dir,
            format,
            amend,
            "c/",
            "i/",
            false,
            out,
            lazy_fetch,
        )?;
        out.extend_from_slice(b"--------------------------------------------------\n");
        out.extend_from_slice(b"Changes not staged for commit:\n");
        append_commit_diff_index_patch(
            cli_session,
            git_dir,
            format,
            amend,
            "i/",
            "w/",
            true,
            out,
            lazy_fetch,
        )?;
    }
    Ok(())
}

fn append_commit_diff_index_patch(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    format: ObjectFormat,
    amend: bool,
    src_prefix: &str,
    dst_prefix: &str,
    worktree: bool,
    out: &mut Vec<u8>,
    lazy_fetch: bool,
) -> Result<()> {
    let worktree_root = worktree_root_for_git_dir(cli_session, git_dir)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let base_tree = commit_verbose_base_tree(git_dir, format, amend)?;
    let entries = if worktree {
        sley_diff_merge::diff_name_status_index_worktree(&worktree_root, git_dir, format)?
    } else {
        sley_diff_merge::diff_name_status_tree_index_with_options(
            git_dir,
            format,
            &base_tree,
            sley_diff_merge::DiffNameStatusOptions::default(),
        )?
    };
    let abbrev = repository_abbrev(git_dir, format)?.unwrap_or(format.hex_len());
    for entry in entries {
        if entry.old_mode == Some(0o160000) || entry.new_mode == Some(0o160000) {
            continue;
        }
        write_diff_patch_entry(
            out,
            &entry,
            DiffRenderOptions {
                line_indicators: sley_diff_merge::render::LineIndicators::default(),
                suppress_blank_empty: false,
                binary: false,
                anchors: &[],
                allow_textconv: true,
                db: &db,
                lazy_fetch,
                worktree_root: worktree.then_some(worktree_root.as_path()),
                use_worktree_new: worktree,
                format,
                abbrev,
                src_prefix,
                dst_prefix,
                context: 3,
                userdiff: None,
                funcname: None,
                colors: None,
                word_diff: None,
                no_index_contents: None,
                submodule_format: commands::diff_options::SubmoduleDiffFormat::Short,
                submodule_dirt: None,
                ws_error: None,
                color_moved: None,
                interhunk: 0,
                ws_ignore: sley_diff_merge::WsIgnore::default(),
                diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
                ignore_blank_lines: false,
                ignore_regexes: &[],
                line_ranges: None,
                indent_heuristic: true,
            },
        )?;
    }
    Ok(())
}

fn commit_verbose_base_tree(git_dir: &Path, format: ObjectFormat, amend: bool) -> Result<ObjectId> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let store = FileRefStore::new(git_dir, format);
    let head = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => store.read_ref(&name)?,
        direct => direct,
    };
    let Some(RefTarget::Direct(head)) = head else {
        return Ok(ObjectId::empty_tree(format));
    };
    let commit = Commit::parse(format, &db.read_object(&head)?.body)?;
    if !amend {
        return Ok(commit.tree);
    }
    let Some(parent) = commit.parents.first() else {
        return Ok(ObjectId::empty_tree(format));
    };
    let parent_commit = Commit::parse(format, &db.read_object(parent)?.body)?;
    Ok(parent_commit.tree)
}

/// Inputs for [`build_commit_editor_template_block`].
struct CommitTemplateBlock<'a> {
    git_dir: &'a Path,
    worktree_root: &'a Path,
    format: ObjectFormat,
    comment_char: &'a str,
    cleanup_mode: CommitCleanupMode,
    allow_empty_message: bool,
    author: &'a [u8],
    committer: &'a [u8],
    /// `commit --date=...` / reused author ⇒ git's `author_date_is_interesting()`
    /// shows the `Date:` line in the template.
    author_date_interesting: bool,
    /// `--amend` ⇒ the staged summary compares against `HEAD^`.
    amend: bool,
    untracked_override: Option<sley_worktree::StatusUntrackedMode>,
}

/// Build the comment-prefixed block git appends to COMMIT_EDITMSG when an editor
/// is launched with `include_status` (commit.status / --status). Mirrors the
/// `use_editor && include_status` branch of builtin/commit.c `prepare_to_commit`:
/// a blank line, the cleanup hint (or a scissors cut line), the Author/Date/
/// Committer ident lines (each shown only when it differs from the committer
/// default), a blank line, then the long working-tree status — all commented.
fn build_commit_editor_template_block(input: &CommitTemplateBlock) -> Result<Vec<u8>> {
    let CommitTemplateBlock {
        git_dir,
        worktree_root,
        format,
        comment_char,
        cleanup_mode,
        allow_empty_message,
        author,
        committer,
        author_date_interesting,
        amend,
        untracked_override,
    } = *input;

    let mut out: Vec<u8> = Vec::new();
    // builtin/commit.c emits `fprintf(s->fp, "\n")` before the hint.
    out.push(b'\n');

    // The cleanup hint, or — for scissors — the cut line. SPACE/VERBATIM keep
    // their own hint text.
    match cleanup_mode {
        CommitCleanupMode::Scissors => {
            append_scissors_cut_line(&mut out, comment_char);
        }
        CommitCleanupMode::Whitespace => {
            let hint = "Please enter the commit message for your changes. Lines starting\nwith '%s' will be kept; you may remove them yourself if you want to.";
            append_commented_hint(
                &mut out,
                comment_char,
                hint,
                allow_empty_message,
                "An empty message aborts the commit.",
            );
        }
        _ => {
            // ALL (default with editor): empty lines are ignored.
            let hint = "Please enter the commit message for your changes. Lines starting\nwith '%s' will be ignored";
            if allow_empty_message {
                append_commented_hint(&mut out, comment_char, &format!("{hint}."), false, "");
            } else {
                append_commented_hint(
                    &mut out,
                    comment_char,
                    &format!("{hint}, and an empty message aborts the commit."),
                    false,
                    "",
                );
            }
        }
    }

    // Ident block: Author / Date / Committer, each gated on differing from the
    // committer default. The first shown line gets a leading blank comment line
    // (git's `ident_shown++ ? "" : "\n"`).
    let author_id = identity_name_email(author);
    let committer_id = identity_name_email(committer);
    let mut ident_shown = false;
    let mut commented_line = |out: &mut Vec<u8>, text: &str| {
        if !ident_shown {
            out.extend_from_slice(comment_char.as_bytes());
            out.push(b'\n');
            ident_shown = true;
        }
        out.extend_from_slice(comment_char.as_bytes());
        out.push(b' ');
        out.extend_from_slice(text.as_bytes());
        out.push(b'\n');
    };
    if author_id != committer_id {
        commented_line(&mut out, &format!("Author:    {author_id}"));
    }
    if author_date_interesting {
        let date = commit_identity_date(author, &DateMode::Default);
        commented_line(&mut out, &format!("Date:      {date}"));
    }
    if !committer_ident_sufficiently_given() {
        commented_line(&mut out, &format!("Committer: {committer_id}"));
    }
    // "Add new line for clarity" (status_printf_ln(s, ..., "%s", "")).
    out.extend_from_slice(comment_char.as_bytes());
    out.push(b'\n');

    // The long working-tree status, every line commented.
    let status = render_commit_template_status(
        git_dir,
        worktree_root,
        format,
        comment_char,
        amend,
        untracked_override,
    )?;
    out.extend_from_slice(&status);
    Ok(out)
}

/// Append git's commented cleanup hint. `hint` carries a single `%s` placeholder
/// for the comment char (matching the gettext templates); when `with_abort` is
/// set, `abort_line` is appended as a final commented sentence.
fn append_commented_hint(
    out: &mut Vec<u8>,
    comment_char: &str,
    hint: &str,
    with_abort: bool,
    abort_line: &str,
) {
    let text = hint.replace("%s", comment_char);
    let mut full = text;
    if with_abort && !abort_line.is_empty() {
        full.push('\n');
        full.push_str(abort_line);
    }
    append_commented_lines(out, comment_char, &full);
}

/// Comment every line of `text` with `comment_char` (git's
/// `strbuf_add_commented_lines`): non-empty lines get `<char> `, empty lines just
/// `<char>`.
fn append_commented_lines(out: &mut Vec<u8>, comment_char: &str, text: &str) {
    for line in text.split('\n') {
        if line.is_empty() {
            out.extend_from_slice(comment_char.as_bytes());
        } else {
            out.extend_from_slice(comment_char.as_bytes());
            out.push(b' ');
            out.extend_from_slice(line.as_bytes());
        }
        out.push(b'\n');
    }
}

/// git's `wt_status_append_cut_line`: the commented `>8` scissors line followed
/// by the "Do not modify..." explanation.
fn append_scissors_cut_line(out: &mut Vec<u8>, comment_char: &str) {
    let cut = "------------------------ >8 ------------------------";
    out.extend_from_slice(comment_char.as_bytes());
    out.push(b' ');
    out.extend_from_slice(cut.as_bytes());
    out.push(b'\n');
    append_commented_lines(
        out,
        comment_char,
        "Do not modify or remove the line above.\nEverything below it will be ignored.",
    );
}

fn message_has_scissors_cut_line(message: &[u8], comment_char: &str) -> bool {
    commit_locate_scissors(message, comment_char) < message.len()
}

/// Whether the committer identity was explicitly supplied (vs guessed from the
/// system). Mirrors git's `committer_ident_sufficiently_given()`: true when both
/// GIT_COMMITTER_NAME and GIT_COMMITTER_EMAIL are set in the environment.
fn committer_ident_sufficiently_given() -> bool {
    env::var_os("GIT_COMMITTER_NAME").is_some() && env::var_os("GIT_COMMITTER_EMAIL").is_some()
}

/// Render the long working-tree status block (every line commented with
/// `comment_char`) for the COMMIT_EDITMSG template.
fn render_commit_template_status(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    comment_char: &str,
    amend: bool,
    untracked_override: Option<sley_worktree::StatusUntrackedMode>,
) -> Result<Vec<u8>> {
    let config = read_repo_config(git_dir).map_err(report_config_setup_error)?;
    let untracked_mode = untracked_override.unwrap_or_else(|| {
        match config.get("status", None, "showUntrackedFiles") {
            Some("no") | Some("false") | Some("0") | Some("off") => {
                sley_worktree::StatusUntrackedMode::None
            }
            Some("all") => sley_worktree::StatusUntrackedMode::All,
            _ => sley_worktree::StatusUntrackedMode::Normal,
        }
    });
    let mut entries = crate::collect_short_status_with_options(
        worktree_root,
        git_dir,
        format,
        sley_worktree::ShortStatusOptions {
            include_ignored: false,
            ignored_mode: sley_worktree::StatusIgnoredMode::Traditional,
            untracked_mode,
            // git's run_status for amend sets s->reference = "HEAD^1" so the
            // template's "Changes to be committed" is the parent→index diff.
            reference: if amend { Some("HEAD^1") } else { None },
        },
    )?;
    let ignore_resolver = SubmoduleIgnoreResolver::load(git_dir, &config, None)?;
    apply_submodule_ignore(&mut entries, &ignore_resolver);
    let base_ref = if amend { "HEAD^" } else { "HEAD" };
    let submodule_summary = status_submodule_summary(
        git_dir,
        &worktree_root,
        format,
        &config,
        base_ref,
        &ignore_resolver,
    )?;
    let display = StatusLongDisplay {
        commit_preview: true,
        show_stash: false,
        ahead_behind: true,
        // builtin/commit.c sets `s->hints = 0` for the template ("Most hints are
        // counter-productive when the commit has already started") — the
        // parenthetical `(use "git ...")` guidance is suppressed regardless of
        // advice.statusHints.
        hints: false,
        untracked_suppressed: untracked_mode == sley_worktree::StatusUntrackedMode::None,
        // The template ALWAYS comments the status, regardless of
        // status.displayCommentPrefix.
        comment_prefix: Some(comment_char.to_string()),
        submodule_summary,
        sparse_footer: None,
        rename_config: resolve_status_rename_config(&config, None, None),
    };
    let sink = build_status_long_sink(worktree_root, git_dir, format, entries, &display)?;
    let mut buf: Vec<u8> = Vec::new();
    sink.write_to(&mut buf);
    Ok(buf)
}

pub(crate) fn render_commit_editor_status_for_rebase(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    comment_char: &str,
    amend: bool,
) -> Result<Vec<u8>> {
    render_commit_template_status(git_dir, worktree_root, format, comment_char, amend, None)
}

fn print_clean_commit_status(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let _ = (git_dir, format);
    let _ = cmd_commit_long_status_preview(cli_session, false, None);
    Ok(())
}

fn build_commit_author_identity(
    author: Option<&str>,
    date: Option<&str>,
    effective_config: &GitConfig,
) -> Result<Vec<u8>> {
    let (name, email) = if let Some(author) = author {
        parse_commit_author_bytes(&argv_bytes_from_string(author))?
    } else {
        // Same precedence as `commit_identity_from_env`: env var, then
        // author.* config, then user.* config, then the built-in default.
        let env_name = env::var_os("GIT_AUTHOR_NAME").map(argv_bytes_from_os);
        let env_email = env::var_os("GIT_AUTHOR_EMAIL").map(argv_bytes_from_os);
        let mut config = if env_name.is_none() || env_email.is_none() {
            IdentityConfig::Loaded(effective_config)
        } else {
            IdentityConfig::Skip
        };
        let name = env_name
            .or_else(|| {
                identity_config_value_for_role("AUTHOR", "name", &mut config)
                    .map(String::into_bytes)
            })
            .or_else(|| identity_default_value("Git Rs", &mut config).map(String::into_bytes));
        let email = env_email
            .or_else(|| {
                identity_config_value_for_role("AUTHOR", "email", &mut config)
                    .map(String::into_bytes)
            })
            .or_else(|| {
                identity_default_value("sley@example.invalid", &mut config).map(String::into_bytes)
            });
        let (Some(name), Some(email)) = (name, email) else {
            return identity_use_config_only_error();
        };
        (name, email)
    };
    let date = date
        .map(str::to_string)
        .unwrap_or_else(|| env::var("GIT_AUTHOR_DATE").unwrap_or_else(|_| "@0 +0000".into()));
    let date = canonicalize_commit_date(&date);
    validate_commit_identity_name("AUTHOR", &name, &email)?;
    sley_sequencer::format_commit_identity_bytes(&name, &email, &date)
}

/// Resolve git's nickname form of `commit --author=<pattern>`.
///
/// A value that already has a closing contact delimiter is parsed normally by
/// `build_commit_author_identity`. Otherwise git searches every reachable
/// commit, case-insensitively, after applying the repository mailmap and uses
/// the newest matching canonical identity.
fn resolve_commit_author_nickname(
    refs: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    author: Option<&str>,
    replace_objects: bool,
) -> Result<Option<String>> {
    let Some(author) = author else {
        return Ok(None);
    };
    if author.contains('>') {
        return Ok(Some(author.to_string()));
    }

    let pattern = regex::RegexBuilder::new(author)
        .case_insensitive(true)
        .build()
        .map_err(|error| {
            eprintln!("fatal: invalid --author pattern '{author}': {error}");
            GitError::Exit(128)
        })?;
    let mut tips = Vec::new();
    let head_oid = match refs.read_ref("HEAD")? {
        Some(RefTarget::Direct(oid)) => Some(oid),
        Some(RefTarget::Symbolic(name)) => match refs.read_ref(&name)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        },
        None => None,
    };
    if let Some(oid) = head_oid
        && let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid)
    {
        tips.push(commit);
    }
    for reference in refs.list_refs()? {
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        if let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid) {
            tips.push(commit);
        }
    }
    tips.sort_unstable();
    tips.dedup();

    let mailmap =
        commands::utility::Mailmap::load_default(refs.git_dir(), format, replace_objects)?;
    for record in rev_list_walk_commits(db, format, tips, false)? {
        let (name, email) = commit_identity_name_email(&record.commit.author);
        let (name, email) = mailmap.map_user(&name, &email);
        let canonical = format!("{name} <{email}>");
        if pattern.is_match(&canonical) {
            return Ok(Some(canonical));
        }
    }

    commit_invalid_author_error(author).map(|_| None)
}

fn parse_commit_author(author: &str) -> Result<(String, String)> {
    let Some((name, rest)) = author.rsplit_once('<') else {
        return commit_invalid_author_error(author);
    };
    let Some(email) = rest.strip_suffix('>') else {
        return commit_invalid_author_error(author);
    };
    let name = name.trim();
    let email = email.trim();
    if name.is_empty() {
        return commit_invalid_author_error(author);
    }
    Ok((name.to_string(), email.to_string()))
}

fn parse_commit_author_bytes(author: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let Some(open) = author.iter().rposition(|byte| *byte == b'<') else {
        return commit_invalid_author_bytes_error(author);
    };
    let Some(email_with_suffix) = author.get(open + 1..) else {
        return commit_invalid_author_bytes_error(author);
    };
    let Some(email) = email_with_suffix.strip_suffix(b">") else {
        return commit_invalid_author_bytes_error(author);
    };
    let name = author[..open].trim_ascii();
    let email = email.trim_ascii();
    if name.is_empty() {
        return commit_invalid_author_bytes_error(author);
    }
    Ok((name.to_vec(), email.to_vec()))
}

fn commit_invalid_author_error(author: &str) -> Result<(String, String)> {
    eprintln!("fatal: --author '{author}' is not 'Name <email>' and matches no existing author");
    Err(GitError::Exit(128))
}

fn commit_invalid_author_bytes_error<T>(author: &[u8]) -> Result<T> {
    let author = String::from_utf8_lossy(author);
    eprintln!("fatal: --author '{author}' is not 'Name <email>' and matches no existing author");
    Err(GitError::Exit(128))
}

#[cfg(test)]
mod commit_author_tests {
    use super::*;

    #[test]
    fn explicit_author_trims_outer_whitespace_and_allows_empty_email() {
        assert_eq!(
            parse_commit_author_bytes(b"  A Name  < address@example.com >").expect("valid author"),
            (b"A Name".to_vec(), b"address@example.com".to_vec())
        );
        assert_eq!(
            parse_commit_author_bytes(b"A Name <>").expect("empty email is valid"),
            (b"A Name".to_vec(), Vec::new())
        );
    }
}
