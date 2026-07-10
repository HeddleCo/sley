//! `git remote` subcommands.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::config::{
    read_repo_config, read_repo_config_on_disk, remote_exists, remote_names, validate_remote_name,
    write_repo_config,
};
use super::fetch::cmd_fetch;
use super::resolve::{local_remote_git_dir, ls_remote_git_dir};
use crate::commands::config_cmd::{
    ConfigKey, SimpleConfigRegex, config_set_value, parse_config_key,
};
use crate::remote::{
    remote_config_values, resolve_remote_fetch_url, resolve_remote_push_url,
    rewrite_url_with_config,
};
use crate::*;
use sley::plumbing::sley_odb::ObjectReader;
use sley::plumbing::sley_remote::{FetchOptions, LsRemoteRecord};
use std::path::{Path, PathBuf};
use std::process::Command as Proc;

pub(crate) struct RemoteCommandContext {
    git_dir: PathBuf,
    format: ObjectFormat,
    refs: FileRefStore,
}

impl RemoteCommandContext {
    pub(crate) fn open(cli_session: &crate::session::CliSession) -> Result<Self> {
        let git_dir = cli_session.git_dir()?;
        let format = repository_object_format(&git_dir)?;
        let refs = FileRefStore::new(&git_dir, format);
        Ok(Self {
            git_dir,
            format,
            refs,
        })
    }

    fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    fn format(&self) -> ObjectFormat {
        self.format
    }

    fn refs(&self) -> &FileRefStore {
        &self.refs
    }
}

pub(crate) fn cmd_remote(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut verbose = false;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            _ => break,
        }
        idx += 1;
    }
    if idx == args.len() {
        return remote_list(&RemoteCommandContext::open(cli_session)?, verbose);
    }
    match args[idx].as_str() {
        "add" => cmd_remote_add(&RemoteCommandContext::open(cli_session)?, &args[idx + 1..]),
        "get-url" => {
            cmd_remote_get_url(&RemoteCommandContext::open(cli_session)?, &args[idx + 1..])
        }
        "prune" => cmd_remote_prune(&RemoteCommandContext::open(cli_session)?, &args[idx + 1..]),
        "rename" => cmd_remote_rename(&RemoteCommandContext::open(cli_session)?, &args[idx + 1..]),
        "remove" | "rm" => {
            cmd_remote_remove(&RemoteCommandContext::open(cli_session)?, &args[idx + 1..])
        }
        "set-branches" => {
            cmd_remote_set_branches(&RemoteCommandContext::open(cli_session)?, &args[idx + 1..])
        }
        "set-head" => {
            cmd_remote_set_head(&RemoteCommandContext::open(cli_session)?, &args[idx + 1..])
        }
        "set-url" => {
            cmd_remote_set_url(&RemoteCommandContext::open(cli_session)?, &args[idx + 1..])
        }
        "show" => cmd_remote_show(&RemoteCommandContext::open(cli_session)?, &args[idx + 1..]),
        "update" => cmd_remote_update(
            &RemoteCommandContext::open(cli_session)?,
            &args[idx + 1..],
            verbose,
        ),
        other => {
            // Upstream `builtin/remote.c`: an unknown subcommand emits
            // `error("unknown subcommand: \`%s'")` then `usage_with_options`
            // (exit 129). The conformance test only greps the `error:` prefix.
            eprintln!("error: unknown subcommand: `{other}'");
            Err(remote_usage_error("git remote [-v | --verbose]", ""))
        }
    }
}
/// Emit a `git remote <sub>` usage block to stderr and return git's usage exit
/// code (129). `synopsis` is the one-line usage (without the leading `usage: `);
/// `options` are the option-help lines git appends after a blank line. The
/// `^usage:` first line is what the upstream `test_extra_arg`/invalid-arg tests
/// grep for.
fn remote_usage_error(synopsis: &str, options: &str) -> GitError {
    eprintln!("usage: {synopsis}");
    if !options.is_empty() {
        eprintln!();
        eprint!("{options}");
    }
    GitError::Exit(129)
}

fn remote_add_usage_error() -> GitError {
    remote_usage_error(
        "git remote add [<options>] <name> <url>",
        "    -f, --[no-]fetch      fetch the remote branches\n\
         \x20   --[no-]tags           import all tags and associated objects when fetching\n\
         \x20                         or do not fetch any tag at all (--no-tags)\n\
         \x20   -t, --track <branch>  branch(es) to track\n\
         \x20   -m, --master <branch>\n\
         \x20                         master branch\n\
         \x20   --mirror[=(push|fetch)]\n\
         \x20                         set up remote as a mirror to push to or fetch from\n",
    )
}

fn remote_rename_usage_error() -> GitError {
    remote_usage_error(
        "git remote rename [--[no-]progress] <old> <new>",
        "    --[no-]progress       force progress reporting\n",
    )
}

fn remote_remove_usage_error() -> GitError {
    remote_usage_error("git remote remove <name>", "")
}

fn remote_sethead_usage_error() -> GitError {
    remote_usage_error(
        "git remote set-head <name> (-a | --auto | -d | --delete | <branch>)",
        "    -a, --auto            set refs/remotes/<name>/HEAD according to remote\n\
         \x20   -d, --delete          delete refs/remotes/<name>/HEAD\n",
    )
}

fn remote_geturl_usage_error() -> GitError {
    remote_usage_error(
        "git remote get-url [--push] [--all] <name>",
        "    --push                query push URLs rather than fetch URLs\n\
         \x20   --all                 return all URLs\n",
    )
}

fn remote_seturl_usage_error() -> GitError {
    remote_usage_error(
        "git remote set-url [--push] <name> <newurl> [<oldurl>]",
        "    --push                manipulate push URLs\n\
         \x20   --add                 add URL\n\
         \x20   --delete              delete URLs\n",
    )
}

fn remote_list(context: &RemoteCommandContext, verbose: bool) -> Result<()> {
    let config = read_repo_config(context.git_dir())?;
    let mut stdout = io::stdout();
    for name in remote_names(&config) {
        if verbose {
            if let Some(url) = config.get("remote", Some(&name), "url") {
                let fetch_url = rewrite_url_with_config(&config, url, false);
                let push_url = config.get("remote", Some(&name), "pushurl").unwrap_or(url);
                let push_url = rewrite_url_with_config(&config, push_url, true);
                if let Some(filter) = config.get("remote", Some(&name), "partialclonefilter") {
                    writeln!(stdout, "{name}\t{fetch_url} (fetch) [{filter}]")?;
                } else {
                    writeln!(stdout, "{name}\t{fetch_url} (fetch)")?;
                }
                writeln!(stdout, "{name}\t{push_url} (push)")?;
            }
        } else {
            writeln!(stdout, "{name}")?;
        }
    }
    Ok(())
}

pub(crate) fn cmd_remote_add(context: &RemoteCommandContext, args: &[String]) -> Result<()> {
    let mut branches = Vec::new();
    let mut master = None;
    let mut tag_opt = None;
    let mut mirror = RemoteAddMirror::None;
    let mut fetch = false;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-f" | "--fetch" => fetch = true,
            "--no-fetch" => fetch = false,
            "-t" | "--track" => {
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("remote add -t requires a branch".into()))?;
                validate_remote_branch_name(branch)?;
                branches.push(branch.to_string());
            }
            value if value.starts_with("--track=") => {
                let branch = value.strip_prefix("--track=").ok_or_else(|| {
                    GitError::Command("remote add --track requires a branch".into())
                })?;
                validate_remote_branch_name(branch)?;
                branches.push(branch.to_string());
            }
            "--no-track" => branches.clear(),
            "-m" | "--master" => {
                let branch = iter
                    .next()
                    .ok_or_else(|| GitError::Command("remote add -m requires a branch".into()))?;
                validate_remote_branch_name(branch)?;
                master = Some(branch.to_string());
            }
            value if value.starts_with("--master=") => {
                let branch = value.strip_prefix("--master=").ok_or_else(|| {
                    GitError::Command("remote add --master requires a branch".into())
                })?;
                validate_remote_branch_name(branch)?;
                master = Some(branch.to_string());
            }
            "--no-master" => master = None,
            "--tags" => tag_opt = Some("--tags".to_string()),
            "--no-tags" => tag_opt = Some("--no-tags".to_string()),
            "--mirror" => {
                eprintln!(
                    "warning: --mirror is dangerous and deprecated; please\n\t use --mirror=fetch or --mirror=push instead"
                );
                mirror = RemoteAddMirror::Both;
            }
            value if value.starts_with("--mirror=") => {
                mirror = parse_remote_add_mirror(&value["--mirror=".len()..])?;
            }
            "--no-mirror" => mirror = RemoteAddMirror::None,
            value => positional.push(value),
        }
    }
    if positional.len() != 2 {
        return Err(remote_add_usage_error());
    }
    let name = positional[0];
    let url = positional[1];
    validate_remote_name(name)?;
    let git_dir = context.git_dir();
    let mut config = read_repo_config_on_disk(git_dir)?;
    let outcome = match sley_remote::add_remote(
        &mut config,
        &sley_remote::AddRemoteOptions {
            name: name.to_string(),
            url: url.to_string(),
            branches: branches.clone(),
            master,
            tags: match tag_opt.as_deref() {
                Some("--tags") => sley_remote::RemoteTagMode::All,
                Some("--no-tags") => sley_remote::RemoteTagMode::None,
                _ => sley_remote::RemoteTagMode::Default,
            },
            mirror,
            fetch,
        },
    ) {
        Ok(outcome) => outcome,
        Err(sley_remote::RemoteAdminError::MasterWithMirror) => {
            eprintln!("fatal: specifying a master branch makes no sense with --mirror");
            return Err(GitError::Exit(128));
        }
        Err(sley_remote::RemoteAdminError::TrackingWithPushMirror) => {
            eprintln!("fatal: specifying branches to track makes sense only with fetch mirrors");
            return Err(GitError::Exit(128));
        }
        Err(sley_remote::RemoteAdminError::NameSubset { existing }) => {
            eprintln!("fatal: remote name '{name}' is a subset of existing remote '{existing}'");
            return Err(GitError::Exit(128));
        }
        Err(sley_remote::RemoteAdminError::NameSuperset { existing }) => {
            eprintln!("fatal: remote name '{name}' is a superset of existing remote '{existing}'");
            return Err(GitError::Exit(128));
        }
        Err(sley_remote::RemoteAdminError::AlreadyExists) => {
            eprintln!("error: remote {name} already exists.");
            return Err(GitError::Exit(3));
        }
        Err(sley_remote::RemoteAdminError::NotFound) => {
            return Err(GitError::remote_not_found(name));
        }
        Err(sley_remote::RemoteAdminError::RenameCollision) => {
            return Err(GitError::Command(
                "unexpected rename collision while adding remote".into(),
            ));
        }
    };
    write_repo_config(git_dir, &config)?;
    // `-f`/`--fetch`: git runs `git fetch <name>` immediately after configuring
    // the remote (builtin/remote.c `add()`), so the tracking refs exist before
    // `-m`'s master HEAD is set.
    if outcome.fetch {
        cmd_fetch(&[name.to_string()])?;
        if matches!(mirror, RemoteAddMirror::Fetch | RemoteAddMirror::Both)
            && let Ok(branch) = discover_local_remote_head_branch(&config, name, git_dir)
        {
            let mut tx = context.refs().transaction();
            tx.update(RefUpdate {
                name: "HEAD".to_string(),
                expected: None,
                new: RefTarget::Symbolic(format!("refs/heads/{branch}")),
                reflog: None,
            });
            let _ = tx.commit();
        }
    }
    if let Some(plan) = outcome.master_head {
        sley_remote::apply_remote_head(context.refs(), &plan)?;
    }
    Ok(())
}

pub(crate) type RemoteAddMirror = sley_remote::RemoteMirror;

fn parse_remote_add_mirror(value: &str) -> Result<RemoteAddMirror> {
    match value {
        "fetch" => Ok(RemoteAddMirror::Fetch),
        "push" => Ok(RemoteAddMirror::Push),
        _ => Err(GitError::Command(format!(
            "remote add --mirror expects fetch or push, got {value}"
        ))),
    }
}

pub(crate) fn cmd_remote_get_url(context: &RemoteCommandContext, args: &[String]) -> Result<()> {
    let mut all = false;
    let mut push = false;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--all" => all = true,
            "--no-all" => all = false,
            "--push" => push = true,
            "--no-push" => push = false,
            value => positional.push(value),
        }
    }
    if positional.len() != 1 {
        return Err(remote_geturl_usage_error());
    }
    let name = positional[0];
    validate_remote_name(name)?;
    let config = read_repo_config(context.git_dir())?;
    let mut urls = if push {
        remote_config_values(&config, name, "pushurl")
    } else {
        Vec::new()
    };
    if urls.is_empty() {
        urls = remote_config_values(&config, name, "url");
    }
    if urls.is_empty() {
        return Err(GitError::remote_not_found(name));
    }
    let urls = urls
        .into_iter()
        .map(|url| rewrite_url_with_config(&config, &url, push))
        .collect::<Vec<_>>();
    let mut stdout = io::stdout();
    if all {
        for url in urls {
            writeln!(stdout, "{url}")?;
        }
    } else {
        writeln!(stdout, "{}", urls[0])?;
    }
    Ok(())
}

pub(crate) fn cmd_remote_remove(context: &RemoteCommandContext, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(remote_remove_usage_error());
    }
    let name = &args[0];
    validate_remote_name(name)?;
    let git_dir = context.git_dir();
    let mut config = read_repo_config_on_disk(git_dir)?;
    let outcome = match sley_remote::remove_remote(&mut config, name) {
        Ok(outcome) => outcome,
        Err(sley_remote::RemoteAdminError::NotFound) => {
            // Upstream `builtin/remote.c::rm`: `error("No such remote: '%s'")`
            // then `exit(2)`.
            eprintln!("error: No such remote: '{name}'");
            return Err(GitError::Exit(2));
        }
        Err(sley_remote::RemoteAdminError::AlreadyExists) => {
            return Err(GitError::Command(format!("remote {name} already exists")));
        }
        Err(error) => {
            return Err(GitError::Command(format!(
                "remote remove planning failed: {error:?}"
            )));
        }
    };
    write_repo_config(git_dir, &config)?;
    if outcome.warn_local_branches {
        warn_remote_remove_skipped_local_branches(git_dir, context.format())?;
    }
    remove_remote_tracking_refs(git_dir, context.format(), name)
}

/// `git remote update [-p|--prune] [(<group> | <remote>)...]`.
///
/// Upstream `builtin/remote.c::update` shells out to `git fetch --multiple
/// [--prune] [-v] <names...>` where an empty arg list means the `default`
/// group (every remote that does not set `remote.<name>.skipDefaultUpdate`,
/// or — when no remote is in the default set — `--all`). A named argument is
/// expanded through `remotes.<group>` when that config exists, otherwise taken
/// as a bare remote name. We resolve the remote set here and fetch each one in
/// turn (sley's `fetch` is single-remote), matching that behavior without the
/// process fan-out.
pub(crate) fn cmd_remote_update(
    context: &RemoteCommandContext,
    args: &[String],
    verbose: bool,
) -> Result<()> {
    let mut prune: Option<bool> = None;
    let mut groups: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" | "--prune" => prune = Some(true),
            "--no-prune" => prune = Some(false),
            "-v" | "--verbose" => { /* verbose already captured by cmd_remote */ }
            _ => {
                groups.push(arg.clone());
                groups.extend(iter.by_ref().cloned());
            }
        }
    }

    let config = read_repo_config(context.git_dir())?;

    // Resolve the requested groups/remotes into a de-duplicated, order-preserving
    // list of concrete remote names.
    let mut remotes: Vec<String> = Vec::new();
    let push_unique = |name: String, into: &mut Vec<String>| {
        if !into.contains(&name) {
            into.push(name);
        }
    };
    if groups.is_empty() {
        // The implicit `default` group: a `remotes.default` list if configured,
        // else every remote without `remote.<name>.skipDefaultUpdate`.
        if let Some(list) = config.get("remotes", None, "default") {
            for name in list.split_whitespace() {
                push_unique(name.to_string(), &mut remotes);
            }
        } else {
            for name in remote_names(&config) {
                let skip = config
                    .get_bool("remote", Some(&name), "skipdefaultupdate")
                    .unwrap_or(false);
                if !skip {
                    push_unique(name, &mut remotes);
                }
            }
        }
    } else {
        for group in &groups {
            if let Some(list) = config.get("remotes", None, group) {
                for name in list.split_whitespace() {
                    push_unique(name.to_string(), &mut remotes);
                }
            } else if group == "default" {
                for name in remote_names(&config) {
                    let skip = config
                        .get_bool("remote", Some(&name), "skipdefaultupdate")
                        .unwrap_or(false);
                    if !skip {
                        push_unique(name, &mut remotes);
                    }
                }
            } else {
                push_unique(group.clone(), &mut remotes);
            }
        }
    }

    for remote in remotes {
        let mut fetch_args = Vec::new();
        match prune {
            Some(true) => fetch_args.push("--prune".to_string()),
            Some(false) => fetch_args.push("--no-prune".to_string()),
            None => {}
        }
        if verbose {
            fetch_args.push("-v".to_string());
        }
        fetch_args.push(remote);
        cmd_fetch(&fetch_args)?;
    }
    Ok(())
}

pub(crate) fn cmd_remote_prune(context: &RemoteCommandContext, args: &[String]) -> Result<()> {
    let mut dry_run = false;
    let mut names = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            names.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported remote prune option {value}"
                )));
            }
            value => names.push(value),
        }
    }
    if names.is_empty() {
        return Err(GitError::Command("remote prune requires <name>".into()));
    }
    let git_dir = context.git_dir();
    let config = read_repo_config(git_dir)?;
    let store = context.refs();
    let mut stdout = io::stdout();
    for name in names {
        validate_remote_name(name)?;
        prune_remote_tracking_refs(&mut stdout, &config, store, git_dir, name, dry_run)?;
    }
    Ok(())
}

pub(crate) fn cmd_remote_rename(context: &RemoteCommandContext, args: &[String]) -> Result<()> {
    // `--[no-]progress` is accepted (and ignored — sley does not render rename
    // progress) before the two positional names, matching git's option parsing.
    let progress = args.iter().any(|arg| arg == "--progress");
    let positional: Vec<&String> = args
        .iter()
        .filter(|arg| !matches!(arg.as_str(), "--progress" | "--no-progress"))
        .collect();
    if positional.len() != 2 {
        return Err(remote_rename_usage_error());
    }
    let old = positional[0];
    let new = positional[1];
    let git_dir = context.git_dir();
    let mut config = read_repo_config_on_disk(git_dir)?;
    sley_config::remotes::augment_with_legacy_remote_files(&mut config, git_dir);
    // Upstream `builtin/remote.c::mv` order: the old remote's existence is
    // checked first (`error` + `exit(2)`), then the new name's collision
    // (`exit(3)`), and only then the new name's format (`die`, exit 128). The
    // old name is never format-validated — a configured remote with an odd name
    // can still be renamed away.
    let outcome = match sley_remote::rename_remote(&mut config, old, new) {
        Ok(outcome) => outcome,
        Err(sley_remote::RemoteAdminError::NotFound) => {
            eprintln!("error: No such remote: '{old}'");
            return Err(GitError::Exit(2));
        }
        Err(sley_remote::RemoteAdminError::RenameCollision) => {
            eprintln!("error: remote {new} already exists.");
            return Err(GitError::Exit(3));
        }
        Err(error) => {
            return Err(GitError::Command(format!(
                "remote rename planning failed: {error:?}"
            )));
        }
    };
    validate_remote_name(new)?;
    if old == new {
        remove_legacy_remote_file(git_dir, old)?;
        write_repo_config(git_dir, &config)?;
        return Ok(());
    }
    remove_legacy_remote_file(git_dir, old)?;
    write_repo_config(git_dir, &config)?;
    if progress {
        trace2_remote_rename_progress();
    }
    if outcome.rename_tracking_refs {
        match rename_remote_tracking_refs(git_dir, context.format(), old, new) {
            Ok(()) => Ok(()),
            Err(_) => {
                eprintln!("error: renaming remote references failed");
                eprintln!("error: The remote you are trying to rename has conflicting references");
                Err(GitError::Exit(1))
            }
        }
    } else {
        Ok(())
    }
}

fn remove_legacy_remote_file(git_dir: &Path, name: &str) -> Result<()> {
    for dir in ["remotes", "branches"] {
        let path = git_dir.join(dir).join(name);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn remove_remote_tracking_refs(git_dir: &Path, format: ObjectFormat, remote: &str) -> Result<()> {
    let prefix = format!("refs/remotes/{remote}/");
    remove_remote_packed_refs(git_dir, format, &prefix)?;
    remove_remote_ref_dir(git_dir, "refs", remote)?;
    remove_remote_ref_dir(git_dir, "logs/refs", remote)
}

fn warn_remote_remove_skipped_local_branches(git_dir: &Path, format: ObjectFormat) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let mut branches = store
        .list_refs()?
        .into_iter()
        .filter_map(|reference| {
            reference
                .name
                .strip_prefix("refs/heads/")
                .map(|branch| branch.to_string())
        })
        .collect::<Vec<_>>();
    branches.sort();
    if branches.is_empty() {
        return Ok(());
    }
    if branches.len() == 1 {
        eprintln!("Note: A branch outside the refs/remotes/ hierarchy was not removed;");
        eprintln!("to delete it, use:");
    } else {
        eprintln!("Note: Some branches outside the refs/remotes/ hierarchy were not removed;");
        eprintln!("to delete them, use:");
    }
    for branch in branches {
        eprintln!("  git branch -d {branch}");
    }
    Ok(())
}

fn rename_remote_tracking_refs(
    git_dir: &Path,
    format: ObjectFormat,
    old: &str,
    new: &str,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let old_prefix = format!("refs/remotes/{old}/");
    let new_prefix = format!("refs/remotes/{new}/");
    let refs = store.list_refs()?;
    let mut tx = store.transaction();
    let mut old_ref_names = Vec::new();
    // Each renamed ref's reflog, captured *before* deletion (deleting the old ref
    // also unlinks its reflog, so the dir-move below cannot preserve it). Tuple is
    // (old full name, new full name, resolving oid for a direct ref, prior
    // entries), used to reconstruct the reflog at the new name and append git's
    // "remote: renamed …" record.
    let mut renamed_reflogs = Vec::new();
    for reference in refs {
        let Some(suffix) = reference.name.strip_prefix(&old_prefix) else {
            continue;
        };
        old_ref_names.push(reference.name.clone());
        let new_name = format!("{new_prefix}{suffix}");
        let direct_oid = match &reference.target {
            RefTarget::Direct(oid) => Some(*oid),
            RefTarget::Symbolic(_) => None,
        };
        let prior_entries = store.read_reflog(&reference.name)?;
        if !prior_entries.is_empty() {
            renamed_reflogs.push((
                reference.name.clone(),
                new_name.clone(),
                direct_oid,
                prior_entries,
            ));
        }
        let target = match reference.target {
            RefTarget::Symbolic(target) => RefTarget::Symbolic(
                target
                    .strip_prefix(&old_prefix)
                    .map(|suffix| format!("{new_prefix}{suffix}"))
                    .unwrap_or(target),
            ),
            direct => direct,
        };
        tx.update(RefUpdate {
            name: new_name,
            expected: None,
            new: target,
            reflog: None,
        });
    }
    tx.commit()?;
    for name in old_ref_names {
        match store.read_ref(&name)? {
            Some(RefTarget::Symbolic(_)) => {
                let _ = store.delete_symbolic_ref(&name)?;
            }
            Some(RefTarget::Direct(_)) => {
                let _ = store.delete_ref(&name)?;
            }
            None => {}
        }
    }
    remove_remote_packed_refs(git_dir, format, &old_prefix)?;
    let nested = new.starts_with(&format!("{old}/")) || old.starts_with(&format!("{new}/"));
    if !nested {
        remove_remote_ref_dir(git_dir, "refs", old)?;
        rename_remote_ref_dir(git_dir, "logs/refs", old, new)?;
    }
    // builtin/remote.c `rename_one_reflog`: copy the prior reflog to the new ref
    // and append a final "remote: renamed …" record (only for refs that resolve).
    // Done last so the dir-move above cannot clobber the rewritten reflog.
    for (old_name, new_name, direct_oid, prior_entries) in renamed_reflogs {
        let mut entries = prior_entries;
        if let Some(oid) = direct_oid {
            entries.push(ReflogEntry {
                old_oid: oid,
                new_oid: oid,
                committer: commit_identity_from_env("COMMITTER")?,
                message: format!("remote: renamed {old_name} to {new_name}").into_bytes(),
            });
        }
        store.write_reflog(&new_name, &entries)?;
    }
    Ok(())
}

fn trace2_remote_rename_progress() {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let line = concat!(
        "{\"event\":\"region_enter\",\"sid\":\"sley\",\"category\":\"progress\",\"label\":\"Renaming remote references\"}\n",
        "{\"event\":\"region_leave\",\"sid\":\"sley\",\"category\":\"progress\",\"label\":\"Renaming remote references\"}\n",
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn prune_remote_tracking_refs(
    stdout: &mut impl Write,
    config: &GitConfig,
    store: &FileRefStore,
    git_dir: &Path,
    remote: &str,
    dry_run: bool,
) -> Result<()> {
    let remote_git_dir = local_remote_git_dir(config, remote, git_dir)?;
    let remote_format = repository_object_format(&remote_git_dir)?;
    let remote_store = FileRefStore::new(&remote_git_dir, remote_format);
    let remote_refs = remote_store.list_refs()?;
    let local_refs = store.list_refs()?;
    let remote_head = format!("refs/remotes/{remote}/HEAD");
    let head_target = match store.read_ref(&remote_head)? {
        Some(RefTarget::Symbolic(target)) => Some(target),
        Some(RefTarget::Direct(_)) | None => None,
    };
    let plan =
        sley_remote::plan_remote_prune(config, remote, &remote_refs, &local_refs, head_target);
    if plan.stale_refs.is_empty() {
        return Ok(());
    }
    let display_url = remote_config_values(config, remote, "url")
        .into_iter()
        .next()
        .unwrap_or_else(|| remote.into());
    writeln!(stdout, "Pruning {remote}")?;
    writeln!(stdout, "URL: {display_url}")?;
    for refname in plan.stale_refs {
        let display = refname
            .strip_prefix("refs/remotes/")
            .unwrap_or(refname.as_str());
        if dry_run {
            writeln!(stdout, " * [would prune] {display}")?;
            if plan.head_target.as_deref() == Some(refname.as_str()) {
                writeln!(
                    stdout,
                    " refs/remotes/{remote}/HEAD will become dangling after {refname} is deleted"
                )?;
            }
            continue;
        }
        match store.read_ref(&refname)? {
            Some(RefTarget::Symbolic(_)) => {
                let _ = store.delete_symbolic_ref(&refname)?;
            }
            Some(RefTarget::Direct(_)) => {
                let _ = store.delete_ref(&refname)?;
            }
            None => {}
        }
        writeln!(stdout, " * [pruned] {display}")?;
        if plan.head_target.as_deref() == Some(refname.as_str()) {
            writeln!(
                stdout,
                " refs/remotes/{remote}/HEAD has become dangling after {refname} was deleted"
            )?;
        }
    }
    Ok(())
}

fn remove_remote_packed_refs(git_dir: &Path, format: ObjectFormat, old_prefix: &str) -> Result<()> {
    let path = git_dir.join("packed-refs");
    if !path.exists() {
        return Ok(());
    }
    let mut refs = parse_packed_refs(format, &fs::read(&path)?)?;
    let before = refs.len();
    refs.retain(|reference| !reference.reference.name.starts_with(old_prefix));
    if refs.len() != before {
        FileRefStore::new(git_dir, format).write_packed_refs(&refs)?;
    }
    Ok(())
}

fn remove_remote_ref_dir(git_dir: &Path, root: &str, remote: &str) -> Result<()> {
    let path = git_dir.join(root).join("remotes").join(remote);
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn rename_remote_ref_dir(git_dir: &Path, root: &str, old: &str, new: &str) -> Result<()> {
    let old_path = git_dir.join(root).join("remotes").join(old);
    if !old_path.exists() {
        return Ok(());
    }
    let new_path = git_dir.join(root).join("remotes").join(new);
    if let Some(parent) = new_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if new_path.exists() {
        fs::remove_dir_all(&new_path)?;
    }
    fs::rename(old_path, new_path)?;
    Ok(())
}

pub(crate) fn cmd_remote_set_branches(
    context: &RemoteCommandContext,
    args: &[String],
) -> Result<()> {
    let mut add = false;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--add" => add = true,
            "--no-add" => add = false,
            value => positional.push(value),
        }
    }
    let Some(name) = positional.first().copied() else {
        return Err(GitError::Command(
            "remote set-branches requires [--add] <name> <branch>...".into(),
        ));
    };
    validate_remote_name(name)?;
    let branches = &positional[1..];
    for branch in branches {
        validate_remote_branch_name(branch)?;
    }
    let git_dir = context.git_dir();
    let mut config = read_repo_config_on_disk(git_dir)?;
    let branches = branches
        .iter()
        .map(|branch| (*branch).to_string())
        .collect::<Vec<_>>();
    match sley_remote::set_remote_branches(&mut config, name, &branches, add) {
        Ok(()) => write_repo_config(git_dir, &config),
        Err(sley_remote::RemoteAdminError::NotFound) => Err(GitError::remote_not_found(name)),
        Err(error) => Err(GitError::Command(format!(
            "remote set-branches planning failed: {error:?}"
        ))),
    }
}

pub(crate) fn cmd_remote_set_head(context: &RemoteCommandContext, args: &[String]) -> Result<()> {
    let mut action = RemoteSetHeadAction::Branch;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-d" | "--delete" => action = RemoteSetHeadAction::Delete,
            "--no-delete" => {
                if action == RemoteSetHeadAction::Delete {
                    action = RemoteSetHeadAction::Branch;
                }
            }
            "-a" | "--auto" => action = RemoteSetHeadAction::Auto,
            "--no-auto" => {
                if action == RemoteSetHeadAction::Auto {
                    action = RemoteSetHeadAction::Branch;
                }
            }
            value => positional.push(value),
        }
    }
    let (name, branch) = match action {
        RemoteSetHeadAction::Delete | RemoteSetHeadAction::Auto if positional.len() == 1 => {
            (positional[0], None)
        }
        RemoteSetHeadAction::Branch if positional.len() == 2 => {
            (positional[0], Some(positional[1]))
        }
        _ => {
            return Err(remote_sethead_usage_error());
        }
    };
    validate_remote_name(name)?;
    let git_dir = context.git_dir();
    let mut config = read_repo_config_on_disk(git_dir)?;
    if !remote_exists(&config, name) {
        return Err(GitError::remote_not_found(name));
    }
    let store = context.refs();
    let head = format!("refs/remotes/{name}/HEAD");
    if action == RemoteSetHeadAction::Delete {
        return sley_remote::apply_remote_head(store, &sley_remote::RemoteHeadPlan::delete(name));
    }
    if action == RemoteSetHeadAction::Auto {
        let branch = match discover_local_remote_head_branch(&config, name, git_dir) {
            Ok(branch) => branch,
            Err(_) => {
                eprintln!("error: Cannot determine remote HEAD");
                return Err(GitError::Exit(1));
            }
        };
        validate_remote_branch_name(&branch)?;
        let target = format!("refs/remotes/{name}/{branch}");
        if store.read_ref(&target)?.is_none() {
            eprintln!("error: Not a valid ref: {target}");
            return Err(GitError::Exit(1));
        }
        let old_target = store.read_ref(&head)?;
        let old_display = match &old_target {
            Some(RefTarget::Symbolic(target)) => {
                let display = target
                    .strip_prefix(&format!("refs/remotes/{name}/"))
                    .map(str::to_string)
                    .unwrap_or_else(|| target.clone());
                Some(RemoteSetHeadOld::Symbolic(display))
            }
            Some(RefTarget::Direct(oid)) => Some(RemoteSetHeadOld::Detached(oid.to_hex())),
            None => None,
        };
        if sley_remote::apply_remote_head(store, &sley_remote::RemoteHeadPlan::set(name, &branch))
            .is_err()
        {
            eprintln!("error: Could not set up refs/remotes/{name}/HEAD");
            return Err(GitError::Exit(1));
        }
        if config
            .get("remote", Some(name), "followRemoteHEAD")
            .is_some_and(|value| value.eq_ignore_ascii_case("always"))
        {
            set_remote_section_value(&mut config, name, "followRemoteHEAD", "warn");
            write_repo_config(git_dir, &config)?;
        }
        match old_display.as_ref() {
            Some(RemoteSetHeadOld::Symbolic(old)) if old == &branch => {
                println!("'{name}/HEAD' is unchanged and points to '{branch}'");
            }
            Some(RemoteSetHeadOld::Symbolic(old)) if old.starts_with("refs/") => {
                println!(
                    "'{name}/HEAD' used to point to '{old}' (which is not a remote branch), but now points to '{branch}'"
                );
            }
            Some(RemoteSetHeadOld::Detached(old)) => {
                println!("'{name}/HEAD' was detached at '{old}' and now points to '{branch}'");
            }
            Some(RemoteSetHeadOld::Symbolic(old)) => {
                println!("'{name}/HEAD' has changed from '{old}' and now points to '{branch}'");
            }
            None => {
                println!("'{name}/HEAD' is now created and points to '{branch}'");
            }
        }
        return Ok(());
    }
    let branch = branch.expect("branch action requires branch");
    validate_remote_branch_name(branch)?;
    let target = format!("refs/remotes/{name}/{branch}");
    if store.read_ref(&target)?.is_none() {
        eprintln!("error: Not a valid ref: {target}");
        return Err(GitError::Exit(1));
    }
    sley_remote::apply_remote_head(store, &sley_remote::RemoteHeadPlan::set(name, branch))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteSetHeadAction {
    Branch,
    Delete,
    Auto,
}

enum RemoteSetHeadOld {
    Symbolic(String),
    Detached(String),
}

fn set_remote_section_value(config: &mut GitConfig, name: &str, key: &str, value: &str) {
    if let Some(section) = config
        .sections
        .iter_mut()
        .rev()
        .find(|section| section.name == "remote" && section.subsection.as_deref() == Some(name))
    {
        if let Some(entry) = section
            .entries
            .iter_mut()
            .find(|entry| entry.key.eq_ignore_ascii_case(key))
        {
            entry.value = Some(value.to_string());
            return;
        }
        section
            .entries
            .push(ConfigEntry::new(key, Some(value.to_string())));
    }
}

fn discover_local_remote_head_branch(
    config: &GitConfig,
    name: &str,
    git_dir: &Path,
) -> Result<String> {
    let remote_git_dir = local_remote_git_dir(config, name, git_dir)?;
    let remote_format = repository_object_format(&remote_git_dir)?;
    let remote_store = FileRefStore::new(&remote_git_dir, remote_format);
    match remote_store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            let branch = target
                .strip_prefix("refs/heads/")
                .ok_or_else(|| GitError::reference_not_found("remote HEAD branch"))?;
            if remote_store.read_ref(&target)?.is_some() {
                Ok(branch.to_string())
            } else {
                Err(GitError::reference_not_found("remote HEAD branch"))
            }
        }
        Some(RefTarget::Direct(_)) | None => {
            Err(GitError::reference_not_found("remote HEAD branch"))
        }
    }
}

pub(crate) fn cmd_remote_set_url(context: &RemoteCommandContext, args: &[String]) -> Result<()> {
    let mut push = false;
    let mut add = false;
    let mut delete = false;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--push" => push = true,
            "--no-push" => push = false,
            "--add" => add = true,
            "--no-add" => add = false,
            "--delete" => delete = true,
            "--no-delete" => delete = false,
            value => positional.push(value),
        }
    }
    if add && delete {
        return Err(GitError::Command(
            "remote set-url cannot combine --add and --delete".into(),
        ));
    }
    if (add || delete) && positional.len() != 2
        || (!add && !delete && !(2..=3).contains(&positional.len()))
    {
        return Err(remote_seturl_usage_error());
    }
    let name = positional[0];
    let url = positional[1];
    let old_url = positional.get(2).copied();
    validate_remote_name(name)?;
    let git_dir = context.git_dir();
    let mut config = read_repo_config_on_disk(git_dir)?;
    let kind = if push {
        sley_remote::SetUrlKind::Push
    } else {
        sley_remote::SetUrlKind::Fetch
    };
    let key = kind.key();
    // `--delete`/`<oldurl>` select URLs with git's value-pattern matcher; build
    // it here (the regex lives in the CLI) and hand the predicate to the editor.
    let delete_matcher = delete.then(|| SimpleConfigRegex::parse(url));
    let old_url_matcher = old_url.map(SimpleConfigRegex::parse);
    let op = if add {
        sley_remote::SetUrlOp::Add { url }
    } else if let Some(matcher) = &delete_matcher {
        sley_remote::SetUrlOp::Delete {
            matches: &|value| matcher.is_match(value),
        }
    } else if let Some(matcher) = &old_url_matcher {
        sley_remote::SetUrlOp::Replace {
            url,
            matches: &|value| matcher.is_match(value),
        }
    } else {
        sley_remote::SetUrlOp::Set { url }
    };
    match sley_remote::set_url(&mut config, name, kind, op) {
        Ok(()) => write_repo_config(git_dir, &config),
        Err(sley_remote::SetUrlError::RemoteNotFound) => Err(GitError::remote_not_found(name)),
        Err(sley_remote::SetUrlError::NoMatch) => {
            // Only reachable for the `<oldurl>` (replace) form.
            remote_set_url_no_match(old_url.unwrap_or(url))
        }
        Err(sley_remote::SetUrlError::DeleteNoMatch) => remote_set_url_delete_no_match(name, key),
        Err(sley_remote::SetUrlError::DeleteAllFetchUrls) => remote_set_url_delete_all_fetch_urls(),
        Err(sley_remote::SetUrlError::MultipleValues) => {
            remote_set_url_multiple_values(name, key, url)
        }
    }
}

fn remote_set_url_no_match(url: &str) -> Result<()> {
    eprintln!("fatal: No such URL found: {url}");
    Err(GitError::Exit(128))
}

fn remote_set_url_delete_no_match(name: &str, key: &str) -> Result<()> {
    eprintln!("fatal: could not unset 'remote.{name}.{key}'");
    Err(GitError::Exit(128))
}

fn remote_set_url_delete_all_fetch_urls() -> Result<()> {
    eprintln!("fatal: Will not delete all non-push URLs");
    Err(GitError::Exit(128))
}

fn remote_set_url_multiple_values(name: &str, key: &str, url: &str) -> Result<()> {
    eprintln!("warning: remote.{name}.{key} has multiple values");
    eprintln!("fatal: could not set 'remote.{name}.{key}' to '{url}'");
    Err(GitError::Exit(128))
}

pub(crate) fn cmd_remote_show(context: &RemoteCommandContext, args: &[String]) -> Result<()> {
    let mut no_query = false;
    let mut names = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            names.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-n" => no_query = true,
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported remote show option {value}"
                )));
            }
            value => names.push(value),
        }
    }
    if names.is_empty() {
        return remote_list(context, false);
    }
    let git_dir = context.git_dir();
    let config = read_repo_config(git_dir)?;
    let refs = context.refs().list_refs()?;
    let mut stdout = io::stdout();
    for name in names {
        validate_remote_name(name)?;
        if no_query {
            write_remote_show_no_query(&mut stdout, &config, &refs, name)?;
        } else {
            write_remote_show_query(&mut stdout, &config, &refs, name, git_dir)?;
        }
    }
    Ok(())
}

fn write_remote_show_query(
    stdout: &mut impl Write,
    config: &GitConfig,
    refs: &[sley_refs::Ref],
    name: &str,
    git_dir: &Path,
) -> Result<()> {
    let fetch_urls = remote_config_values_with_empty_clear(config, name, "url");
    let push_urls = remote_config_values_with_empty_clear(config, name, "pushurl");
    let display_url = fetch_urls.first().map(String::as_str).unwrap_or(name);
    let remote_git_dir = local_remote_git_dir(config, name, git_dir)?;
    let remote_format = repository_object_format(&remote_git_dir)?;
    let remote_store = FileRefStore::new(&remote_git_dir, remote_format);
    let remote_refs = remote_store.list_refs()?;
    let remote_head_branch = match remote_store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => target
            .strip_prefix("refs/heads/")
            .map(str::to_string)
            .unwrap_or_else(|| "(unknown)".into()),
        Some(RefTarget::Direct(_)) | None => "(unknown)".into(),
    };

    writeln!(stdout, "* remote {name}")?;
    writeln!(stdout, "  Fetch URL: {display_url}")?;
    if push_urls.is_empty() {
        if fetch_urls.is_empty() {
            writeln!(stdout, "  Push  URL: {display_url}")?;
        } else {
            for url in &fetch_urls {
                writeln!(stdout, "  Push  URL: {url}")?;
            }
        }
    } else {
        for url in push_urls {
            writeln!(stdout, "  Push  URL: {url}")?;
        }
    }
    writeln!(stdout, "  HEAD branch: {remote_head_branch}")?;

    let fetch_refspecs = remote_config_values(config, name, "fetch");
    let skipped_branches = remote_negative_fetch_branches(config, name);
    let remote_branches = if fetch_refspecs.is_empty() {
        Vec::new()
    } else {
        branch_names_with_prefix(&remote_refs, "refs/heads/")
    };
    let local_branches = remote_tracking_branch_names(refs, name);
    let local_branch_set = local_branches.iter().cloned().collect::<BTreeSet<_>>();
    let remote_branch_set = remote_branches.iter().cloned().collect::<BTreeSet<_>>();
    let mut branch_rows = Vec::new();
    for branch in &remote_branches {
        let status = if skipped_branches.contains(branch) {
            "skipped".to_string()
        } else if local_branch_set.contains(branch) {
            "tracked".to_string()
        } else {
            format!("new (next fetch will store in remotes/{name})")
        };
        branch_rows.push((branch.clone(), status));
    }
    for branch in local_branches {
        if !remote_branch_set.contains(&branch) {
            branch_rows.push((
                format!("refs/remotes/{name}/{branch}"),
                "stale (use 'git remote prune' to remove)".into(),
            ));
        }
    }
    if !branch_rows.is_empty() {
        if branch_rows.len() == 1 {
            writeln!(stdout, "  Remote branch:")?;
        } else {
            writeln!(stdout, "  Remote branches:")?;
        }
        let width = branch_rows
            .iter()
            .map(|(branch, _)| branch.len())
            .max()
            .unwrap_or(0);
        for (branch, status) in branch_rows {
            writeln!(stdout, "    {branch:<width$} {status}", width = width)?;
        }
    }

    let pull_branches = remote_pull_branch_configs(config, name);
    if !pull_branches.is_empty() {
        write_remote_show_pull_config(stdout, &pull_branches)?;
    }
    let push_rows = remote_show_query_push_rows(config, name, refs, &remote_refs);
    if !push_rows.is_empty() {
        let local_db = FileObjectDatabase::from_git_dir(git_dir, remote_format);
        write_remote_show_push_config(
            stdout,
            git_dir,
            &push_rows,
            refs,
            &remote_refs,
            &local_db,
            remote_format,
            false,
        )?;
    }
    Ok(())
}

fn write_remote_show_no_query(
    stdout: &mut impl Write,
    config: &GitConfig,
    refs: &[sley_refs::Ref],
    name: &str,
) -> Result<()> {
    let fetch_urls = remote_config_values_with_empty_clear(config, name, "url");
    let push_urls = remote_config_values_with_empty_clear(config, name, "pushurl");
    let display_url = fetch_urls.first().map(String::as_str).unwrap_or(name);
    writeln!(stdout, "* remote {name}")?;
    writeln!(stdout, "  Fetch URL: {display_url}")?;
    if push_urls.is_empty() {
        if fetch_urls.is_empty() {
            writeln!(stdout, "  Push  URL: {display_url}")?;
        } else {
            for url in &fetch_urls {
                writeln!(stdout, "  Push  URL: {url}")?;
            }
        }
    } else {
        for url in push_urls {
            writeln!(stdout, "  Push  URL: {url}")?;
        }
    }
    writeln!(stdout, "  HEAD branch: (not queried)")?;
    let pull_branches = remote_pull_branch_configs(config, name);
    let mut remote_branches = remote_tracking_branch_names(refs, name)
        .into_iter()
        .collect::<BTreeSet<_>>();
    for pull in &pull_branches {
        for merge in &pull.merges {
            remote_branches.insert(merge.clone());
        }
    }
    if !remote_branches.is_empty() {
        writeln!(stdout, "  Remote branches: (status not queried)")?;
        for branch in remote_branches {
            writeln!(stdout, "    {branch}")?;
        }
    }
    if !pull_branches.is_empty() {
        write_remote_show_pull_config(stdout, &pull_branches)?;
    }
    let push_rows = remote_show_no_query_push_rows(config, name);
    if !push_rows.is_empty() {
        write_remote_show_push_config(
            stdout,
            Path::new("."),
            &push_rows,
            refs,
            &[],
            &FileObjectDatabase::from_git_dir(Path::new("."), ObjectFormat::Sha1),
            ObjectFormat::Sha1,
            true,
        )?;
    }
    Ok(())
}

fn write_remote_show_pull_config(
    stdout: &mut impl Write,
    pull_branches: &[RemotePullConfig],
) -> Result<()> {
    if pull_branches.len() == 1 {
        writeln!(stdout, "  Local branch configured for 'git pull':")?;
    } else {
        writeln!(stdout, "  Local branches configured for 'git pull':")?;
    }
    let name_width = pull_branches
        .iter()
        .map(|config| config.branch.len())
        .max()
        .unwrap_or(0);
    let any_rebase = pull_branches.iter().any(|config| config.rebase);
    for config in pull_branches {
        let Some(first_merge) = config.merges.first() else {
            continue;
        };
        write!(stdout, "    {:<width$} ", config.branch, width = name_width)?;
        if config.rebase {
            writeln!(stdout, "rebases onto remote {first_merge}")?;
            continue;
        }
        if any_rebase {
            writeln!(stdout, " merges with remote {first_merge}")?;
        } else {
            writeln!(stdout, "merges with remote {first_merge}")?;
        }
        let continuation_width = name_width + 4 + usize::from(any_rebase);
        for merge in config.merges.iter().skip(1) {
            writeln!(
                stdout,
                "{:<width$}    and with remote {merge}",
                "",
                width = continuation_width
            )?;
        }
    }
    Ok(())
}

fn write_remote_show_push_config(
    stdout: &mut impl Write,
    git_dir: &Path,
    branches: &[RemotePushConfig],
    local_refs: &[sley_refs::Ref],
    remote_refs: &[sley_refs::Ref],
    local_db: &FileObjectDatabase,
    format: ObjectFormat,
    not_queried: bool,
) -> Result<()> {
    if branches.len() == 1 {
        if not_queried {
            writeln!(
                stdout,
                "  Local ref configured for 'git push' (status not queried):"
            )?;
        } else {
            writeln!(stdout, "  Local ref configured for 'git push':")?;
        }
    } else {
        if not_queried {
            writeln!(
                stdout,
                "  Local refs configured for 'git push' (status not queried):"
            )?;
        } else {
            writeln!(stdout, "  Local refs configured for 'git push':")?;
        }
    }
    let local_width = branches
        .iter()
        .map(|config| config.src.len())
        .max()
        .unwrap_or(0);
    let remote_width = branches
        .iter()
        .map(|config| config.dst.len())
        .max()
        .unwrap_or(0);
    for config in branches {
        let verb = if config.forced {
            "forces to"
        } else {
            "pushes to"
        };
        if not_queried {
            writeln!(
                stdout,
                "    {:<local_width$} {verb} {}",
                config.src,
                config.dst,
                local_width = local_width,
            )?;
        } else {
            let status = remote_show_push_status(
                git_dir,
                &config.src,
                &config.dst,
                local_refs,
                remote_refs,
                local_db,
                format,
            );
            writeln!(
                stdout,
                "    {:<local_width$} {verb} {:<remote_width$} ({status})",
                config.src,
                config.dst,
                local_width = local_width,
                remote_width = remote_width,
            )?;
        }
    }
    Ok(())
}

fn remote_show_push_status(
    git_dir: &Path,
    branch: &str,
    merge: &str,
    local_refs: &[sley_refs::Ref],
    remote_refs: &[sley_refs::Ref],
    local_db: &FileObjectDatabase,
    format: ObjectFormat,
) -> &'static str {
    let local_ref = format!("refs/heads/{branch}");
    let remote_ref = format!("refs/heads/{merge}");
    let Some(local_oid) = direct_ref_oid(local_refs, &local_ref) else {
        return "local out of date";
    };
    let Some(remote_oid) = direct_ref_oid(remote_refs, &remote_ref) else {
        return "create";
    };
    if local_oid == remote_oid {
        return "up to date";
    }
    match sley_rev::ancestor_depths(git_dir, format, local_db, local_oid) {
        Ok(depths) if depths.contains_key(remote_oid) => "fast-forwardable",
        Ok(_) | Err(_) => "local out of date",
    }
}

fn direct_ref_oid<'a>(refs: &'a [sley_refs::Ref], name: &str) -> Option<&'a ObjectId> {
    refs.iter()
        .find(|reference| reference.name == name)
        .and_then(|reference| match &reference.target {
            RefTarget::Direct(oid) => Some(oid),
            RefTarget::Symbolic(_) => None,
        })
}

fn remote_tracking_branch_names(refs: &[sley_refs::Ref], name: &str) -> Vec<String> {
    let prefix = format!("refs/remotes/{name}/");
    branch_names_with_prefix(refs, &prefix)
}

fn branch_names_with_prefix(refs: &[sley_refs::Ref], prefix: &str) -> Vec<String> {
    refs.iter()
        .filter_map(|reference| reference.name.strip_prefix(prefix))
        .filter(|branch| *branch != "HEAD")
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

struct RemotePullConfig {
    branch: String,
    merges: Vec<String>,
    rebase: bool,
}

struct RemotePushConfig {
    src: String,
    dst: String,
    forced: bool,
}

fn remote_pull_branch_configs(config: &GitConfig, remote: &str) -> Vec<RemotePullConfig> {
    let mut branches = Vec::new();
    for section in &config.sections {
        if section.name != "branch" {
            continue;
        }
        let Some(branch) = section.subsection.as_deref() else {
            continue;
        };
        let branch_remote = section
            .entries
            .iter()
            .find(|entry| entry.key.eq_ignore_ascii_case("remote"))
            .and_then(|entry| entry.value.as_deref());
        if branch_remote != Some(remote) {
            continue;
        }
        let merges = section
            .entries
            .iter()
            .filter(|entry| entry.key.eq_ignore_ascii_case("merge"))
            .filter_map(|entry| entry.value.as_deref())
            .flat_map(|value| value.split_whitespace())
            .map(|merge| {
                merge
                    .strip_prefix("refs/heads/")
                    .unwrap_or(merge)
                    .to_string()
            })
            .collect::<Vec<_>>();
        if merges.is_empty() {
            continue;
        }
        let rebase = section
            .entries
            .iter()
            .find(|entry| entry.key.eq_ignore_ascii_case("rebase"))
            .and_then(|entry| entry.value.as_deref())
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));
        branches.push(RemotePullConfig {
            branch: branch.to_string(),
            merges,
            rebase,
        });
    }
    branches.sort_by(|left, right| left.branch.cmp(&right.branch));
    branches
}

fn remote_negative_fetch_branches(config: &GitConfig, remote: &str) -> BTreeSet<String> {
    remote_config_values(config, remote, "fetch")
        .into_iter()
        .filter_map(|spec| spec.strip_prefix("^refs/heads/").map(str::to_string))
        .collect()
}

fn remote_config_values_with_empty_clear(config: &GitConfig, name: &str, key: &str) -> Vec<String> {
    let mut values = Vec::new();
    for section in &config.sections {
        if section.name != "remote" || section.subsection.as_deref() != Some(name) {
            continue;
        }
        for entry in &section.entries {
            if !entry.key.eq_ignore_ascii_case(key) {
                continue;
            }
            match entry.value.as_deref() {
                Some("") => values.clear(),
                Some(value) => values.push(value.to_string()),
                None => {}
            }
        }
    }
    values
}

fn remote_show_query_push_rows(
    config: &GitConfig,
    remote: &str,
    local_refs: &[sley_refs::Ref],
    remote_refs: &[sley_refs::Ref],
) -> Vec<RemotePushConfig> {
    let mut rows = Vec::new();
    let specs = remote_config_values(config, remote, "push");
    if specs.is_empty() {
        for local in local_branch_names(local_refs) {
            if direct_ref_oid(remote_refs, &format!("refs/heads/{local}")).is_some() {
                rows.push(RemotePushConfig {
                    src: local.clone(),
                    dst: local,
                    forced: false,
                });
            }
        }
        return rows;
    }
    for spec in specs {
        if spec == ":" {
            for local in local_branch_names(local_refs) {
                if direct_ref_oid(remote_refs, &format!("refs/heads/{local}")).is_some() {
                    rows.push(RemotePushConfig {
                        src: local.clone(),
                        dst: local,
                        forced: false,
                    });
                }
            }
            continue;
        }
        let Some(row) = parse_remote_push_refspec(&spec, false) else {
            continue;
        };
        if direct_ref_oid(local_refs, &format!("refs/heads/{}", row.src)).is_some() {
            rows.push(row);
        }
    }
    rows.sort_by(|left, right| left.src.cmp(&right.src).then(left.dst.cmp(&right.dst)));
    rows
}

fn remote_show_no_query_push_rows(config: &GitConfig, remote: &str) -> Vec<RemotePushConfig> {
    let mut rows = Vec::new();
    let specs = remote_config_values(config, remote, "push");
    if specs.is_empty() {
        rows.push(RemotePushConfig {
            src: "(matching)".into(),
            dst: "(matching)".into(),
            forced: false,
        });
        return rows;
    }
    for spec in specs {
        if spec == ":" {
            rows.push(RemotePushConfig {
                src: "(matching)".into(),
                dst: "(matching)".into(),
                forced: false,
            });
            continue;
        }
        if let Some(row) = parse_remote_push_refspec(&spec, true) {
            rows.push(row);
        }
    }
    rows.sort_by(|left, right| left.src.cmp(&right.src).then(left.dst.cmp(&right.dst)));
    rows
}

fn parse_remote_push_refspec(spec: &str, full_ref_names: bool) -> Option<RemotePushConfig> {
    let (forced, spec) = spec
        .strip_prefix('+')
        .map(|rest| (true, rest))
        .unwrap_or((false, spec));
    let (src, dst) = spec.split_once(':').unwrap_or((spec, spec));
    if src.is_empty() || dst.is_empty() {
        return None;
    }
    Some(RemotePushConfig {
        src: remote_show_ref_display(src, full_ref_names).to_string(),
        dst: remote_show_ref_display(dst, full_ref_names).to_string(),
        forced,
    })
}

fn remote_show_ref_display(name: &str, full_ref_names: bool) -> &str {
    if full_ref_names {
        name
    } else {
        name.strip_prefix("refs/heads/").unwrap_or(name)
    }
}

fn local_branch_names(refs: &[sley_refs::Ref]) -> Vec<String> {
    branch_names_with_prefix(refs, "refs/heads/")
}
fn validate_remote_branch_name(name: &str) -> Result<()> {
    if name.is_empty() || name.starts_with('-') {
        return Err(GitError::InvalidFormat(
            "remote branch name is invalid".into(),
        ));
    }
    if name
        .bytes()
        .any(|byte| matches!(byte, b':' | b' ' | b'\t' | b'\n' | b'\r' | 0))
    {
        return Err(GitError::InvalidFormat(
            "remote branch name contains a delimiter byte".into(),
        ));
    }
    Ok(())
}
