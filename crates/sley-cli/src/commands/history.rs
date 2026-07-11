//! `git history` native history-editing commands.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, Write};

use sley::plumbing::sley_sequencer::history::{
    HistoryRefScope, HistoryRewordAnalysis, HistoryRewordRequest, HistorySplitAnalysis,
    HistorySplitRequest, HistorySplitSelection, analyze_history_reword, analyze_history_split,
    execute_history_reword, execute_history_split, validate_history_reword_targets,
    validate_history_split_targets, write_history_split_tree,
};

use crate::*;

const SPLIT_USAGE: &str =
    "git history split <commit> [--dry-run] [--update-refs=(branches|head)] [--] [<pathspec>...]";
const REWORD_USAGE: &str =
    "git history reword <commit> [--dry-run] [--update-refs=(branches|head)]";

pub(crate) fn cmd_history(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("split") => cmd_history_split(cli_session, &args[1..]),
        Some("reword") => cmd_history_reword(cli_session, &args[1..]),
        _ => {
            eprintln!("usage: {REWORD_USAGE}");
            eprintln!("   or: {SPLIT_USAGE}");
            Err(GitError::Exit(129))
        }
    }
}

fn cmd_history_reword(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut dry_run = false;
    let mut scope = HistoryRefScope::Branches;
    let mut commit = None;
    for arg in args {
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "--update-refs=branches" => scope = HistoryRefScope::Branches,
            "--update-refs=head" => scope = HistoryRefScope::Head,
            value if value.starts_with("--update-refs=") => {
                return history_error("--update-refs expects one of 'branches' or 'head'");
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{value}`");
                eprintln!("usage: {REWORD_USAGE}");
                return Err(GitError::Exit(129));
            }
            value if commit.is_none() => commit = Some(value.to_string()),
            _ => {
                eprintln!("usage: {REWORD_USAGE}");
                return Err(GitError::Exit(129));
            }
        }
    }
    let Some(commit_name) = commit else {
        return history_error("command expects a committish");
    };

    let context = crate::repository::RepositoryContext::from_session(cli_session)?;
    let oid = match context.resolve_revision(&commit_name) {
        Ok(oid) => match sley_rev::peel_to_commit(context.objects(), context.format(), &oid) {
            Ok(commit) => commit,
            Err(_) => return history_error(&format!("commit cannot be found: {commit_name}")),
        },
        Err(_) => return history_error(&format!("commit cannot be found: {commit_name}")),
    };
    let analysis = analyze_history_reword(context.objects(), context.format(), oid)?;
    if let Err(error) = validate_history_reword_targets(
        context.git_dir(),
        context.format(),
        context.objects(),
        oid,
        scope,
    ) {
        return match error {
            GitError::Command(message) => history_error(&message),
            error => Err(error),
        };
    }
    let message = edit_reword_message(context.git_dir(), &analysis)?;
    let effective_config = crate::identity_effective_config_for(cli_session).unwrap_or_default();
    let committer = crate::commit_identity_from_env("COMMITTER", &effective_config)?;
    let request = HistoryRewordRequest {
        analysis,
        message,
        committer,
        reflog_message: b"reword: updating HEAD".to_vec(),
        scope,
        dry_run,
    };
    match execute_history_reword(
        context.git_dir(),
        context.format(),
        context.objects(),
        request,
    ) {
        Ok(outcome) => {
            if dry_run {
                for (name, old, new) in outcome.updated_refs {
                    println!("update {name} {new} {old}");
                }
            }
            Ok(())
        }
        Err(GitError::Command(message)) => history_error(&message),
        Err(error) => Err(error),
    }
}

fn edit_reword_message(git_dir: &Path, analysis: &HistoryRewordAnalysis) -> Result<Vec<u8>> {
    let path = git_dir.join("COMMIT_EDITMSG");
    let comment = commands::replay::comment_char(git_dir);
    let mut template = analysis.original.message.clone();
    while template.last().is_some_and(u8::is_ascii_whitespace) {
        template.pop();
    }
    template.extend_from_slice(b"\n\n");
    template.push(comment);
    template.extend_from_slice(
        b" Please enter the commit message for the reworded changes. Lines starting\n",
    );
    template.push(comment);
    template.extend_from_slice(b" with '");
    template.push(comment);
    template.extend_from_slice(b"' will be ignored, and an empty message aborts the commit.\n");
    template.push(comment);
    template.extend_from_slice(b" Changes to be committed:\n");
    for changed in &analysis.changed_paths {
        let kind = match (
            analysis.parent_entries.get(changed),
            analysis.original_entries.get(changed),
        ) {
            (None, Some(_)) => "new file:   ",
            (Some(_), None) => "deleted:    ",
            _ => "modified:   ",
        };
        template.push(comment);
        template.push(b'\t');
        template.extend_from_slice(kind.as_bytes());
        template.extend_from_slice(changed);
        template.push(b'\n');
    }
    template.push(comment);
    template.push(b'\n');
    fs::write(&path, template)?;
    commands::replay::launch_editor(git_dir, &path)?;
    let message = commands::replay::strip_comment_lines(&fs::read(&path)?, comment);
    if message.is_empty() {
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    Ok(message)
}

fn cmd_history_split(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut dry_run = false;
    let mut scope = HistoryRefScope::Branches;
    let mut commit = None;
    let mut pathspecs = Vec::new();
    let mut after_dashdash = false;
    for arg in args {
        if after_dashdash {
            pathspecs.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => after_dashdash = true,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "--update-refs=branches" => scope = HistoryRefScope::Branches,
            "--update-refs=head" => scope = HistoryRefScope::Head,
            value if value.starts_with("--update-refs=") => {
                return history_error("--update-refs expects one of 'branches' or 'head'");
            }
            value if value.starts_with('-') && commit.is_none() => {
                eprintln!("error: unknown option `{value}`");
                eprintln!("usage: {SPLIT_USAGE}");
                return Err(GitError::Exit(129));
            }
            value if commit.is_none() => commit = Some(value.to_string()),
            value => pathspecs.push(value.to_string()),
        }
    }
    let Some(commit_name) = commit else {
        return history_error("command expects a committish");
    };

    let context = crate::repository::RepositoryContext::from_session(cli_session)?;
    let oid = match context.resolve_revision(&commit_name) {
        Ok(oid) => match sley_rev::peel_to_commit(context.objects(), context.format(), &oid) {
            Ok(commit) => commit,
            Err(_) => return history_error(&format!("commit cannot be found: {commit_name}")),
        },
        Err(_) => return history_error(&format!("commit cannot be found: {commit_name}")),
    };
    let analysis = match analyze_history_split(context.objects(), context.format(), oid) {
        Ok(analysis) => analysis,
        Err(GitError::Command(message)) => return history_error(&message),
        Err(error) => return Err(error),
    };
    if let Err(error) = validate_history_split_targets(
        context.git_dir(),
        context.format(),
        context.objects(),
        &analysis,
        scope,
    ) {
        return match error {
            GitError::Command(message) => history_error(&message),
            error => Err(error),
        };
    }

    let offered_paths = analysis
        .changed_paths
        .iter()
        .filter(|path| history_pathspec_matches(path, &pathspecs))
        .cloned()
        .collect::<Vec<_>>();
    let offered = history_split_units(&analysis, &offered_paths);
    let selected = select_history_changes(&offered)?;
    let selected_paths = selected
        .iter()
        .filter(|selection| selection.mode || selection.content)
        .map(|selection| selection.path.clone())
        .collect::<BTreeSet<_>>();
    let split_tree =
        write_history_split_tree(context.objects(), context.format(), &analysis, &selected)?;
    if split_tree == analysis.parent_tree {
        return history_error("split commit is empty");
    }
    if split_tree == analysis.original_tree {
        return history_error("split commit tree matches original commit");
    }

    let first_message = edit_split_message(context.git_dir(), &analysis, &selected_paths, true)?;
    let second_message = edit_split_message(context.git_dir(), &analysis, &selected_paths, false)?;
    let effective_config = crate::identity_effective_config_for(cli_session).unwrap_or_default();
    let committer = crate::commit_identity_from_env("COMMITTER", &effective_config)?;
    let request = HistorySplitRequest {
        analysis,
        split_tree,
        first_message,
        second_message,
        committer,
        reflog_message: format!("split: updating {commit_name}").into_bytes(),
        scope,
        dry_run,
    };
    match execute_history_split(
        context.git_dir(),
        context.format(),
        context.objects(),
        request,
    ) {
        Ok(outcome) => {
            if dry_run {
                for (name, old, new) in outcome.updated_refs {
                    println!("update {name} {new} {old}");
                }
            }
            Ok(())
        }
        Err(GitError::Command(message)) => history_error(&message),
        Err(error) => Err(error),
    }
}

fn history_pathspec_matches(path: &[u8], pathspecs: &[String]) -> bool {
    pathspecs.is_empty()
        || pathspecs.iter().any(|spec| {
            let spec = spec.trim_end_matches('/').as_bytes();
            path == spec || (path.starts_with(spec) && path.get(spec.len()) == Some(&b'/'))
        })
}

#[derive(Debug, Clone, Copy)]
enum HistorySplitUnitKind {
    Whole,
    Mode,
    Content,
}

#[derive(Debug, Clone)]
struct HistorySplitUnit {
    path: Vec<u8>,
    kind: HistorySplitUnitKind,
}

fn history_split_units(
    analysis: &HistorySplitAnalysis,
    paths: &[Vec<u8>],
) -> Vec<HistorySplitUnit> {
    let mut units = Vec::new();
    for path in paths {
        match (
            analysis.parent_entries.get(path),
            analysis.original_entries.get(path),
        ) {
            (Some(before), Some(after)) if before.mode != after.mode && before.oid != after.oid => {
                units.push(HistorySplitUnit {
                    path: path.clone(),
                    kind: HistorySplitUnitKind::Mode,
                });
                units.push(HistorySplitUnit {
                    path: path.clone(),
                    kind: HistorySplitUnitKind::Content,
                });
            }
            _ => units.push(HistorySplitUnit {
                path: path.clone(),
                kind: HistorySplitUnitKind::Whole,
            }),
        }
    }
    units
}

fn select_history_changes(units: &[HistorySplitUnit]) -> Result<Vec<HistorySplitSelection>> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut selected = BTreeMap::<Vec<u8>, HistorySplitSelection>::new();
    for unit in units {
        print!("Stage this hunk [y,n,q,a,d,e,p,?]? ");
        io::stdout().flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        if matches!(line.trim().chars().next(), Some('y' | 'Y' | 'a' | 'A')) {
            let selection =
                selected
                    .entry(unit.path.clone())
                    .or_insert_with(|| HistorySplitSelection {
                        path: unit.path.clone(),
                        mode: false,
                        content: false,
                    });
            match unit.kind {
                HistorySplitUnitKind::Whole => {
                    selection.mode = true;
                    selection.content = true;
                }
                HistorySplitUnitKind::Mode => selection.mode = true,
                HistorySplitUnitKind::Content => selection.content = true,
            }
        }
    }
    Ok(selected.into_values().collect())
}

fn edit_split_message(
    git_dir: &Path,
    analysis: &HistorySplitAnalysis,
    selected: &BTreeSet<Vec<u8>>,
    first: bool,
) -> Result<Vec<u8>> {
    let path = git_dir.join("COMMIT_EDITMSG");
    let mut template = analysis.original.message.clone();
    while template.last().is_some_and(u8::is_ascii_whitespace) {
        template.pop();
    }
    template.extend_from_slice(b"\n\n# Please enter the commit message for the split-out changes. Lines starting\n# with '#' will be ignored, and an empty message aborts the commit.\n# Changes to be committed:\n");
    for changed in &analysis.changed_paths {
        if selected.contains(changed) != first {
            continue;
        }
        let before = if first {
            analysis.parent_entries.get(changed)
        } else if selected.contains(changed) {
            analysis.original_entries.get(changed)
        } else {
            analysis.parent_entries.get(changed)
        };
        let after = analysis.original_entries.get(changed);
        let kind = match (before, after) {
            (None, Some(_)) => "new file:   ",
            (Some(_), None) => "deleted:    ",
            _ => "modified:   ",
        };
        template.extend_from_slice(b"#\t");
        template.extend_from_slice(kind.as_bytes());
        template.extend_from_slice(changed);
        template.push(b'\n');
    }
    template.extend_from_slice(b"#\n");
    fs::write(&path, template)?;
    commands::replay::launch_editor(git_dir, &path)?;
    let message = commands::replay::strip_comment_lines(
        &fs::read(&path)?,
        commands::replay::comment_char(git_dir),
    );
    if message.is_empty() {
        eprintln!("Aborting commit due to empty commit message.");
        return Err(GitError::Exit(1));
    }
    Ok(message)
}

fn history_error<T>(message: &str) -> Result<T> {
    eprintln!("error: {message}");
    Err(GitError::Exit(1))
}
