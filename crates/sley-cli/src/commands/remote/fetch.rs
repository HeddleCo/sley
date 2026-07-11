//! Fetch command, transport, and submodule recursion.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::FetchRecurseSubmodules;
use super::clone::{normalize_clone_filter, parse_clone_depth, register_promisor_remote};
use super::config::{
    clone_effective_config_value, read_repo_config, remote_exists, remote_names, write_repo_config,
};
use super::pack::{
    configured_legacy_protocol, configured_protocol_version, prettify_refname,
    trace_configured_local_protocol_version, trace_protocol_v2_ls_refs_request,
    trace2_local_transfer_negotiation, unique_abbrev,
};
use super::resolve::{RemoteCommandContext, local_remote_git_dir, ls_remote_git_dir};
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

fn fetch_repository_plan(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    requested_remote: Option<&str>,
) -> Result<sley_remote::FetchRepositoryPlan> {
    let current_branch = FileRefStore::new(git_dir, format).current_branch()?;
    Ok(sley_remote::plan_fetch_repository(
        config,
        current_branch.as_deref(),
        requested_remote,
    ))
}

pub(super) fn default_fetch_remote(context: &RemoteCommandContext) -> Result<String> {
    let git_dir = context.required_git_dir()?;
    let format = repository_object_format(git_dir)?;
    Ok(fetch_repository_plan(git_dir, format, context.required_config()?, None)?.remote)
}

pub(crate) fn cmd_fetch(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut source = None::<String>;
    let mut refspecs = Vec::new();
    let mut options = FetchOptions {
        quiet: false,
        auto_follow_tags: true,
        fetch_all_tags: false,
        prune: false,
        prune_tags: false,
        dry_run: false,
        force: false,
        append: false,
        write_fetch_head: true,
        tag_option_explicit: false,
        prune_option_explicit: false,
        prune_tags_option_explicit: false,
        refmap: None,
        depth: None,
        merge_srcs: Vec::new(),
        filter: None,
        filter_auto: false,
        refetch: false,
        cloning: false,
        record_promisor_refs: true,
        update_shallow: false,
        reject_shallow: false,
        deepen_relative: false,
        update_head_ok: false,
        deepen_since: None,
        deepen_not: Vec::new(),
        ssh_options: None,
        upload_pack_command: None,
        atomic: false,
        negotiation_restrict: None,
        negotiation_include: None,
    };
    let mut unshallow = false;
    let mut filter_option_explicit = false;
    // The raw `--filter` spec (e.g. "blob:none"), retained so a filtered fetch
    // of a named remote can register it as a promisor remote afterwards (git's
    // `partial_clone_register` records the spec under
    // `remote.<name>.partialclonefilter`).
    let mut filter_spec = None::<String>;
    let mut prefetch = false;
    let mut recurse_submodules_cli = FetchRecurseSubmodules::Default;
    let mut recurse_submodules_default = FetchRecurseSubmodules::OnDemand;
    let mut submodule_prefix = String::new();
    let mut jobs = None::<usize>;
    let mut upload_pack_command = None::<String>;
    let mut server_options = Vec::<String>::new();
    let mut server_options_from_cli = false;
    // `git fetch --all`/`--multiple`: fetch from a resolved list of remotes.
    let mut fetch_all_remotes = None::<bool>;
    let mut fetch_multiple = false;
    let mut set_upstream = false;
    let mut read_refspecs_from_stdin = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--all" if source.is_none() => fetch_all_remotes = Some(true),
            "--no-all" if source.is_none() => fetch_all_remotes = Some(false),
            "--multiple" | "-m" if source.is_none() => fetch_multiple = true,
            "--no-multiple" if source.is_none() => fetch_multiple = false,
            "-q" | "--quiet" => options.quiet = true,
            "--no-quiet" => options.quiet = false,
            "--write-fetch-head" => options.write_fetch_head = true,
            "--no-write-fetch-head" => options.write_fetch_head = false,
            "--append" | "-a" => options.append = true,
            "--no-append" => options.append = false,
            "-n" | "--dry-run" => options.dry_run = true,
            "--no-dry-run" => options.dry_run = false,
            "-f" | "--force" => options.force = true,
            "--no-force" => options.force = false,
            "-k" | "--keep" => {}
            "--atomic" => options.atomic = true,
            "--no-atomic" => options.atomic = false,
            "--depth" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("fetch --depth requires a value".into()))?;
                options.depth = Some(parse_clone_depth(value)?);
            }
            value if value.starts_with("--depth=") => {
                let value = value
                    .strip_prefix("--depth=")
                    .ok_or_else(|| GitError::Command("fetch --depth requires a value".into()))?;
                options.depth = Some(parse_clone_depth(value)?);
            }
            "--prune" | "-p" => {
                options.prune = true;
                options.prune_option_explicit = true;
            }
            "--no-prune" => {
                options.prune = false;
                options.prune_option_explicit = true;
            }
            "--prune-tags" | "-P" => {
                options.prune_tags = true;
                options.prune_tags_option_explicit = true;
            }
            "--no-prune-tags" => {
                options.prune_tags = false;
                options.prune_tags_option_explicit = true;
            }
            "--refmap" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("fetch --refmap requires a value".into()))?;
                push_fetch_refmap(&mut options, value);
            }
            value if value.starts_with("--refmap=") => {
                let value = value.strip_prefix("--refmap=").unwrap_or_default();
                push_fetch_refmap(&mut options, value);
            }
            "--tags" | "-t" => {
                options.auto_follow_tags = true;
                options.fetch_all_tags = true;
                options.tag_option_explicit = true;
            }
            "--no-tags" => {
                options.auto_follow_tags = false;
                options.fetch_all_tags = false;
                options.tag_option_explicit = true;
            }
            "--filter" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("fetch --filter requires a value".into()))?;
                let normalized = normalize_clone_filter(value)?;
                options.filter_auto = normalized == "auto";
                options.filter = if options.filter_auto {
                    None
                } else {
                    fetch_pack_filter_from_spec(&normalized)
                };
                filter_spec = Some(normalized);
                filter_option_explicit = true;
            }
            value if value.starts_with("--filter=") => {
                let value = value
                    .strip_prefix("--filter=")
                    .ok_or_else(|| GitError::Command("fetch --filter requires a value".into()))?;
                let normalized = normalize_clone_filter(value)?;
                options.filter_auto = normalized == "auto";
                options.filter = if options.filter_auto {
                    None
                } else {
                    fetch_pack_filter_from_spec(&normalized)
                };
                filter_spec = Some(normalized);
                filter_option_explicit = true;
            }
            "--no-filter" => {
                options.filter = None;
                options.filter_auto = false;
                filter_spec = None;
                filter_option_explicit = true;
            }
            "--refetch" => options.refetch = true,
            "--no-refetch" => options.refetch = false,
            "--prefetch" => prefetch = true,
            "--no-prefetch" => prefetch = false,
            "--stdin" => read_refspecs_from_stdin = true,
            "--recurse-submodules" => recurse_submodules_cli = FetchRecurseSubmodules::On,
            "--no-recurse-submodules" => recurse_submodules_cli = FetchRecurseSubmodules::Off,
            value if value.starts_with("--recurse-submodules=") => {
                let value = value.strip_prefix("--recurse-submodules=").ok_or_else(|| {
                    GitError::Command("fetch --recurse-submodules requires a value".into())
                })?;
                recurse_submodules_cli = FetchRecurseSubmodules::from_arg(Some(value))?;
            }
            "--recurse-submodules-default" => {
                recurse_submodules_default = FetchRecurseSubmodules::OnDemand;
            }
            value if value.starts_with("--recurse-submodules-default=") => {
                let value = value
                    .strip_prefix("--recurse-submodules-default=")
                    .ok_or_else(|| {
                        GitError::Command(
                            "fetch --recurse-submodules-default requires a value".into(),
                        )
                    })?;
                recurse_submodules_default = FetchRecurseSubmodules::from_arg(Some(value))?;
            }
            "--submodule-prefix" => {
                submodule_prefix = iter
                    .next()
                    .ok_or_else(|| {
                        GitError::Command("fetch --submodule-prefix requires a value".into())
                    })?
                    .clone();
            }
            value if value.starts_with("--submodule-prefix=") => {
                submodule_prefix = value
                    .strip_prefix("--submodule-prefix=")
                    .unwrap_or_default()
                    .to_string();
            }
            "-j" | "--jobs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("fetch --jobs requires a value".into()))?;
                jobs = parse_fetch_jobs(value)?;
            }
            value if value.starts_with("-j") && value.len() > 2 => {
                jobs = parse_fetch_jobs(&value[2..])?;
            }
            value if value.starts_with("--jobs=") => {
                jobs = parse_fetch_jobs(value.strip_prefix("--jobs=").unwrap_or_default())?;
            }
            "--negotiation-tip" | "--negotiation-restrict" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command(format!("fetch {} requires a value", arg.as_str()))
                })?;
                push_negotiation_value(&mut options.negotiation_restrict, value);
            }
            value
                if value.starts_with("--negotiation-tip=")
                    || value.starts_with("--negotiation-restrict=") =>
            {
                let (_, value) = value.split_once('=').unwrap_or((value, ""));
                push_negotiation_value(&mut options.negotiation_restrict, value);
            }
            "--negotiation-include" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("fetch --negotiation-include requires a value".into())
                })?;
                push_negotiation_value(&mut options.negotiation_include, value);
            }
            value if value.starts_with("--negotiation-include=") => {
                let value = value
                    .strip_prefix("--negotiation-include=")
                    .unwrap_or_default();
                push_negotiation_value(&mut options.negotiation_include, value);
            }
            "--upload-pack" | "--exec" => {
                upload_pack_command = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command(format!("fetch {} requires a value", arg.as_str()))
                        })?
                        .to_string(),
                );
            }
            value if value.starts_with("--upload-pack=") || value.starts_with("--exec=") => {
                let (_, command) = value.split_once('=').unwrap_or((value, ""));
                upload_pack_command = Some(command.to_string());
            }
            "-o" | "--server-option" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("fetch --server-option requires a value".into())
                })?;
                server_options.push(value.clone());
                server_options_from_cli = true;
            }
            value if value.starts_with("--server-option=") => {
                let value = value.strip_prefix("--server-option=").unwrap_or_default();
                server_options.push(value.to_string());
                server_options_from_cli = true;
            }
            "--no-server-option" => {
                server_options.clear();
                server_options_from_cli = true;
            }
            "--unshallow" => unshallow = true,
            "--set-upstream" => set_upstream = true,
            "--no-set-upstream" => set_upstream = false,
            "-u" | "--update-head-ok" => options.update_head_ok = true,
            "--update-shallow" => options.update_shallow = true,
            "--deepen" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("fetch --deepen requires a value".into()))?;
                options.depth = Some(parse_clone_depth(value)?);
                options.deepen_relative = true;
            }
            value if value.starts_with("--deepen=") => {
                let value = value
                    .strip_prefix("--deepen=")
                    .ok_or_else(|| GitError::Command("fetch --deepen requires a value".into()))?;
                options.depth = Some(parse_clone_depth(value)?);
                options.deepen_relative = true;
            }
            "--shallow-since" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("fetch --shallow-since requires a value".into())
                })?;
                options.deepen_since = Some(parse_shallow_since(value)?);
            }
            value if value.starts_with("--shallow-since=") => {
                let value = value.strip_prefix("--shallow-since=").unwrap_or_default();
                options.deepen_since = Some(parse_shallow_since(value)?);
            }
            "--shallow-exclude" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("fetch --shallow-exclude requires a value".into())
                })?;
                options.deepen_not.push(value.clone());
            }
            value if value.starts_with("--shallow-exclude=") => {
                let value = value.strip_prefix("--shallow-exclude=").unwrap_or_default();
                options.deepen_not.push(value.to_string());
            }
            // `git fetch <remote> tag <name>` is shorthand for the refspec
            // `refs/tags/<name>:refs/tags/<name>` (builtin/fetch.c). Only after a
            // remote has been seen, so a remote literally named "tag" still works.
            "tag" if source.is_some() => {
                let name = iter
                    .next()
                    .ok_or_else(|| GitError::Command("you need to specify a tag name".into()))?;
                refspecs.push(format!("refs/tags/{name}:refs/tags/{name}"));
            }
            // `OPT_IPVERSION` in builtin/fetch.c: accepted but a no-op for the
            // file:// transport; the `--no-` forms are undefined in git and so
            // fall through to the unknown-option path below.
            "-4" | "--ipv4" | "-6" | "--ipv6" => {}
            // An unrecognized dash-option is a usage error (git's parse-options
            // emits `error: unknown option …` and exits 129), not a remote name.
            // `-` alone is git's stdin marker and is left to the positional path.
            value if value.starts_with('-') && value != "-" => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                eprintln!("usage: git fetch [<options>] [<repository> [<refspec>...]]");
                return Err(GitError::Exit(129));
            }
            _ if source.is_none() => source = Some(arg.clone()),
            _ => refspecs.push(rewrite_empty_source_refspec(arg)),
        }
    }
    if read_refspecs_from_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        refspecs.extend(
            input
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(rewrite_empty_source_refspec),
        );
    }
    options.upload_pack_command = upload_pack_command.clone();
    let context = RemoteCommandContext::require_repository(cli_session)?;
    let repository = context.required_repository()?;
    let cwd = context.cwd();
    let git_dir = repository.git_dir();
    let format = repository.object_format();
    // A bare repo with no working tree never has a "checked out" branch, so the
    // current-branch fetch refusal is keyed off whether a *non-bare* worktree
    // shares the symref (`find_shared_symref` skips bare worktrees) rather than a
    // blanket update-head-ok for every bare repo — otherwise a bare repo's linked
    // worktree branch could be overwritten by fetch (t5516 #120).
    let config = context.required_config()?;
    let transport_config = repo_config_with_transport_policy(&context, git_dir)?;
    let current_branch = repository.references().current_branch()?;
    let all_from_config = source.is_none()
        && fetch_all_remotes.is_none()
        && config.get_bool("fetch", None, "all").unwrap_or(false);
    let fetch_all_remotes = fetch_all_remotes.unwrap_or(false) || all_from_config;
    if fetch_multiple && fetch_all_remotes {
        eprintln!("fatal: --multiple and --all cannot be used together");
        return Err(GitError::Exit(128));
    }
    if fetch_all_remotes {
        if source.is_some() {
            eprintln!("fatal: fetch --all does not take a repository argument");
            return Err(GitError::Exit(128));
        }
        if !refspecs.is_empty() {
            eprintln!("fatal: fetch --all does not make sense with refspecs");
            return Err(GitError::Exit(128));
        }
        let remotes = fetch_all_remote_names(&config);
        fetch_multiple_remotes(FetchMultipleRequest {
            git_dir,
            format,
            worktree_root: cwd,
            config,
            current_branch: current_branch.as_deref(),
            resolution: context.resolution(),
            command_context: &context,
            transport_config: &transport_config,
            remotes,
            refspecs: &refspecs,
            options: &options,
            prefetch,
            filter_option_explicit,
            recurse_submodules_cli,
            recurse_submodules_default,
            submodule_prefix: &submodule_prefix,
            jobs,
            server_options: &server_options,
            server_options_from_cli,
        })?;
        if options.refetch {
            trace2_fetch_refetch_maintenance();
        }
        return Ok(());
    }
    if fetch_multiple && source.is_none() {
        fetch_multiple = false;
    }
    if fetch_multiple {
        let mut names = Vec::new();
        names.push(source.take().unwrap());
        names.extend(refspecs.drain(..));
        let remotes = resolve_remote_or_group_names(&config, &names)?;
        fetch_multiple_remotes(FetchMultipleRequest {
            git_dir,
            format,
            worktree_root: cwd,
            config,
            current_branch: current_branch.as_deref(),
            resolution: context.resolution(),
            command_context: &context,
            transport_config: &transport_config,
            remotes,
            refspecs: &refspecs,
            options: &options,
            prefetch,
            filter_option_explicit,
            recurse_submodules_cli,
            recurse_submodules_default,
            submodule_prefix: &submodule_prefix,
            jobs,
            server_options: &server_options,
            server_options_from_cli,
        })?;
        if options.refetch {
            trace2_fetch_refetch_maintenance();
        }
        return Ok(());
    }
    // With no remote argument, resolve the default the way git's
    // `remote_for_branch` does: the current branch's `branch.<name>.remote`,
    // else the sole configured remote, else `origin`.
    let repository_plan =
        sley_remote::plan_fetch_repository(config, current_branch.as_deref(), source.as_deref());
    let source = repository_plan.remote;
    // When no refspecs are given on the command line and the current branch's
    // `branch.<name>.remote` is the remote we're fetching, git's get_ref_map adds
    // the branch's `branch.<name>.merge` ref(s) as the FETCH_HEAD for-merge
    // entries (`add_merge_config`). Resolve those so the configured-refspec fetch
    // marks them correctly (and `pull` can find its merge target).
    if refspecs.is_empty() && !prefetch {
        options.merge_srcs = repository_plan.merge_srcs;
    }
    if unshallow {
        if options.depth.is_some() {
            eprintln!("fatal: --depth and --unshallow cannot be used together");
            return Err(GitError::Exit(128));
        }
        if !git_dir.join("shallow").exists() {
            eprintln!("fatal: --unshallow on a complete repository does not make sense");
            return Err(GitError::Exit(128));
        }
        options.depth = Some(sley_remote::INFINITE_DEPTH);
    }
    if !filter_option_explicit {
        sley_remote::apply_configured_partial_clone_filter(config, &source, &mut options);
    }
    let effective_refspecs = if prefetch {
        prefetch_refspecs(config, &source, &refspecs)
    } else {
        refspecs.clone()
    };
    if prefetch {
        options.refmap = Some(Vec::new());
    }
    if fetch_raw_oid_refspecs(
        git_dir,
        format,
        &source,
        &effective_refspecs,
        &options,
        filter_spec.as_deref(),
        context.resolution(),
        &transport_config,
    )? {
        return Ok(());
    }
    let before_fetch_refs = fetch_ref_snapshot(git_dir, format)?;
    let refetch = options.refetch;
    let effective_server_options = if server_options_from_cli {
        server_options
    } else {
        configured_server_options(config, &source)?
    };
    if server_options_from_cli && configured_legacy_protocol(Some(config)) {
        eprintln!("fatal: server options require protocol version 2 or later");
        eprintln!("fatal: see protocol.version in 'git help config' for more details");
        return Err(GitError::Exit(128));
    }
    let result = fetch_one_source_with_outcome(
        git_dir,
        format,
        &source,
        &effective_refspecs,
        options.clone(),
        &effective_server_options,
        &context,
        &transport_config,
    );
    if result.is_ok() && refetch {
        trace2_fetch_refetch_maintenance();
    }
    let outcome = result?;
    // A successful `fetch --filter` against a configured remote registers that
    // remote as a promisor remote (git's `partial_clone_register`): the partial
    // pack is only usable once the repo knows the remote can supply the omitted
    // objects on demand.
    if let Some(spec) = filter_spec.as_deref() {
        register_promisor_remote(git_dir, &source, spec)?;
    }
    if set_upstream {
        fetch_set_upstream_from_outcome(git_dir, format, &source, &outcome)?;
    }
    let refreshed_config = filter_spec
        .as_ref()
        .map(|_| read_repo_config(git_dir))
        .transpose()?;
    let config = refreshed_config.as_ref().unwrap_or(config);
    trace2_local_transfer_negotiation(config, upload_pack_command.as_deref());
    let recurse_submodules = resolve_fetch_recurse_submodules(
        &config,
        recurse_submodules_cli,
        recurse_submodules_default,
    );
    fetch_populated_submodules_after_superproject(FetchSubmoduleRequest {
        git_dir,
        format,
        worktree_root: cwd,
        runtime_cwd: cwd,
        config,
        recurse_submodules,
        default_recurse_submodules: recurse_submodules_default,
        source: &source,
        changed_gitlinks: changed_gitlinks_for_fetch(
            git_dir,
            format,
            &before_fetch_refs,
            &outcome,
        )?,
        options: &options,
        submodule_prefix: &submodule_prefix,
        jobs,
    })?;
    Ok(())
}

struct FetchMultipleRequest<'a> {
    git_dir: &'a Path,
    format: ObjectFormat,
    worktree_root: &'a Path,
    config: &'a GitConfig,
    current_branch: Option<&'a str>,
    resolution: sley_remote::RemoteResolutionContext<'a>,
    command_context: &'a RemoteCommandContext,
    transport_config: &'a GitConfig,
    remotes: Vec<String>,
    refspecs: &'a [String],
    options: &'a FetchOptions,
    prefetch: bool,
    filter_option_explicit: bool,
    recurse_submodules_cli: FetchRecurseSubmodules,
    recurse_submodules_default: FetchRecurseSubmodules,
    submodule_prefix: &'a str,
    jobs: Option<usize>,
    server_options: &'a [String],
    server_options_from_cli: bool,
}

fn fetch_multiple_remotes(req: FetchMultipleRequest<'_>) -> Result<()> {
    if req.server_options_from_cli && configured_legacy_protocol(Some(req.config)) {
        eprintln!("fatal: server options require protocol version 2 or later");
        eprintln!("fatal: see protocol.version in 'git help config' for more details");
        return Err(GitError::Exit(128));
    }
    trace_fetch_parallel_jobs(req.jobs.unwrap_or(1));
    let parallel_fetch = req.jobs.is_some_and(|jobs| jobs > 1) && req.remotes.len() > 1;
    let mut failed = false;
    for remote in req.remotes {
        if !req.options.quiet {
            println!("Fetching {remote}");
        }
        let mut remote_options = req.options.clone();
        remote_options.append = true;
        if !req.filter_option_explicit {
            sley_remote::apply_configured_partial_clone_filter(
                req.config,
                &remote,
                &mut remote_options,
            );
        }
        if req.refspecs.is_empty() && !req.prefetch {
            remote_options.merge_srcs =
                sley_remote::plan_fetch_repository(req.config, req.current_branch, Some(&remote))
                    .merge_srcs;
        }
        let effective_refspecs = if req.prefetch {
            prefetch_refspecs(req.config, &remote, req.refspecs)
        } else {
            req.refspecs.to_vec()
        };
        if req.prefetch {
            remote_options.refmap = Some(Vec::new());
        }
        let before_fetch_refs = fetch_ref_snapshot(req.git_dir, req.format)?;
        let remote_server_options = if req.server_options_from_cli {
            req.server_options.to_vec()
        } else {
            match configured_server_options(req.config, &remote) {
                Ok(options) => options,
                Err(err) => {
                    print_fetch_failure(&remote, &err, parallel_fetch);
                    failed = true;
                    continue;
                }
            }
        };
        let result = fetch_one_source_with_outcome(
            req.git_dir,
            req.format,
            &remote,
            &effective_refspecs,
            remote_options.clone(),
            &remote_server_options,
            req.command_context,
            req.transport_config,
        );
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(err) => {
                print_fetch_failure(&remote, &err, parallel_fetch);
                failed = true;
                continue;
            }
        };
        let recurse_submodules = resolve_fetch_recurse_submodules(
            req.config,
            req.recurse_submodules_cli,
            req.recurse_submodules_default,
        );
        fetch_populated_submodules_after_superproject(FetchSubmoduleRequest {
            git_dir: req.git_dir,
            format: req.format,
            worktree_root: req.worktree_root,
            runtime_cwd: req.resolution.cwd,
            config: req.config,
            recurse_submodules,
            default_recurse_submodules: req.recurse_submodules_default,
            source: &remote,
            changed_gitlinks: changed_gitlinks_for_fetch(
                req.git_dir,
                req.format,
                &before_fetch_refs,
                &outcome,
            )?,
            options: &remote_options,
            submodule_prefix: req.submodule_prefix,
            jobs: req.jobs,
        })?;
    }
    if failed {
        return Err(GitError::Exit(1));
    }
    trace_fetch_maintenance();
    Ok(())
}

fn print_fetch_failure(remote: &str, err: &GitError, parallel_fetch: bool) {
    if parallel_fetch {
        eprintln!("could not fetch '{remote}' (exit code: 128)");
        return;
    }
    eprintln!("error: could not fetch {remote}");
    print_fetch_failure_detail(err);
}

fn print_fetch_failure_detail(err: &GitError) {
    match err {
        GitError::Exit(_) => {}
        GitError::Cli(_, message) | GitError::Command(message) => eprintln!("{message}"),
        other => eprintln!("{other}"),
    }
}

fn fetch_all_remote_names(config: &GitConfig) -> Vec<String> {
    remote_names(config)
        .into_iter()
        .filter(|name| {
            !config
                .get_bool("remote", Some(name), "skipfetchall")
                .unwrap_or(false)
        })
        .collect()
}

fn resolve_remote_or_group_names(config: &GitConfig, names: &[String]) -> Result<Vec<String>> {
    let mut remotes = Vec::new();
    for name in names {
        let before = remotes.len();
        if let Some(group) = config.get("remotes", None, name) {
            for remote in group.split_whitespace() {
                push_unique_remote(&mut remotes, remote.to_string());
            }
        }
        if remotes.len() == before {
            if !remote_exists(config, name) {
                eprintln!("fatal: no such remote or remote group: {name}");
                return Err(GitError::Exit(128));
            }
            push_unique_remote(&mut remotes, name.clone());
        }
    }
    Ok(remotes)
}

fn push_unique_remote(remotes: &mut Vec<String>, name: String) {
    if !remotes.contains(&name) {
        remotes.push(name);
    }
}

fn trace_fetch_parallel_jobs(jobs: usize) {
    trace_fetch_line(&format!(
        "trace: run_processes_parallel: preparing to run up to {jobs} tasks\n"
    ));
}

fn trace_fetch_maintenance() {
    trace_fetch_line("trace: built-in: git maintenance run --auto --no-quiet\n");
}

fn fetch_pack_filter_from_spec(spec: &str) -> Option<sley_odb::PackObjectFilter> {
    sley_remote::pack_filter_from_spec(spec)
}

/// `git fetch <remote> :<dst>` is shorthand for fetching the remote's `HEAD`
/// into `<dst>` (git resolves an empty refspec source to `HEAD` in
/// `get_fetch_map`). Rewrite the bare-colon form to an explicit `HEAD:<dst>`
/// refspec, preserving any leading `+` force marker.
fn rewrite_empty_source_refspec(arg: &str) -> String {
    let (force, rest) = match arg.strip_prefix('+') {
        Some(rest) => ("+", rest),
        None => ("", arg),
    };
    if let Some(dst) = rest.strip_prefix(':')
        && !dst.is_empty()
    {
        return format!("{force}HEAD:{dst}");
    }
    arg.to_string()
}

fn push_fetch_refmap(options: &mut FetchOptions, value: &str) {
    let refmap = options.refmap.get_or_insert_with(Vec::new);
    if !value.is_empty() {
        refmap.push(value.to_string());
    }
}

fn push_negotiation_value(values: &mut Option<Vec<String>>, value: &str) {
    let values = values.get_or_insert_with(Vec::new);
    if value.is_empty() {
        values.clear();
    } else {
        values.push(value.to_string());
    }
}

fn parse_fetch_jobs(value: &str) -> Result<Option<usize>> {
    let parsed = value
        .parse::<isize>()
        .map_err(|_| GitError::Command(format!("invalid number of parallel jobs: {value}")))?;
    if parsed < 0 {
        return Err(GitError::Command(format!(
            "negative values not allowed for submodule.fetchJobs"
        )));
    }
    if parsed == 0 {
        Ok(None)
    } else {
        Ok(Some(parsed as usize))
    }
}

pub(crate) fn resolve_fetch_recurse_submodules(
    config: &GitConfig,
    cli: FetchRecurseSubmodules,
    _default_mode: FetchRecurseSubmodules,
) -> FetchRecurseSubmodules {
    if cli != FetchRecurseSubmodules::Default {
        return cli;
    }
    if config
        .get_bool("submodule", None, "recurse")
        .unwrap_or(false)
    {
        return FetchRecurseSubmodules::On;
    }
    if let Some(value) = config.get("fetch", None, "recursesubmodules") {
        let mode = FetchRecurseSubmodules::from_config(value);
        if mode != FetchRecurseSubmodules::Default {
            return mode;
        }
    }
    FetchRecurseSubmodules::Default
}

pub(crate) struct FetchSubmoduleRequest<'a> {
    pub(crate) git_dir: &'a Path,
    pub(crate) format: ObjectFormat,
    pub(crate) worktree_root: &'a Path,
    pub(crate) runtime_cwd: &'a Path,
    pub(crate) config: &'a GitConfig,
    pub(crate) recurse_submodules: FetchRecurseSubmodules,
    pub(crate) default_recurse_submodules: FetchRecurseSubmodules,
    pub(crate) source: &'a str,
    pub(crate) changed_gitlinks: Vec<ChangedGitlink>,
    pub(crate) options: &'a FetchOptions,
    pub(crate) submodule_prefix: &'a str,
    pub(crate) jobs: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangedGitlink {
    path: String,
    oid: ObjectId,
    super_oid: ObjectId,
}

struct CurrentDirGuard {
    previous: PathBuf,
}

impl CurrentDirGuard {
    fn enter(path: &Path, previous: &Path) -> Result<Self> {
        env::set_current_dir(path)?;
        Ok(Self {
            previous: previous.to_path_buf(),
        })
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.previous);
    }
}

pub(crate) fn fetch_populated_submodules_after_superproject(
    req: FetchSubmoduleRequest<'_>,
) -> Result<()> {
    if req.recurse_submodules == FetchRecurseSubmodules::Off {
        return Ok(());
    }
    let mut submodules = crate::commands::submodule::read_submodule_configs(req.worktree_root)?;
    if req.worktree_root.is_dir() {
        submodules.extend(crate::commands::submodule::index_gitlink_submodule_configs(
            req.git_dir,
            req.format,
            &submodules,
        )?);
    }
    if submodules.is_empty() && req.changed_gitlinks.is_empty() {
        return Ok(());
    }
    let jobs = req
        .jobs
        .or_else(|| configured_submodule_fetch_jobs(req.config));
    trace_fetch_submodule_jobs(jobs.unwrap_or(1));
    let mut seen_submodules = std::collections::BTreeSet::new();
    for submodule in submodules {
        if !fetch_submodule_path_is_active(req.config, &submodule.path) {
            continue;
        }
        let mode = fetch_recurse_mode_for_submodule(
            req.config,
            req.worktree_root,
            &submodule,
            req.recurse_submodules,
        );
        let submodule_root = req.worktree_root.join(&submodule.path);
        let Some(sub_git_dir) =
            resolve_submodule_git_dir(req.git_dir, &submodule_root, &submodule.path)
        else {
            continue;
        };
        ensure_submodule_object_store(&sub_git_dir)?;
        let sub_format = repository_object_format(&sub_git_dir)?;
        let nested_config = read_repo_config(&sub_git_dir)?;
        let changed_for_path = req
            .changed_gitlinks
            .iter()
            .filter(|changed| changed.path == submodule.path)
            .collect::<Vec<_>>();
        let should_fetch = match mode {
            FetchRecurseSubmodules::On => true,
            FetchRecurseSubmodules::OnDemand => changed_for_path
                .iter()
                .any(|changed| !submodule_has_commit(&sub_git_dir, sub_format, &changed.oid)),
            FetchRecurseSubmodules::Default => match req.default_recurse_submodules {
                FetchRecurseSubmodules::On => true,
                FetchRecurseSubmodules::OnDemand => changed_for_path
                    .iter()
                    .any(|changed| !submodule_has_commit(&sub_git_dir, sub_format, &changed.oid)),
                FetchRecurseSubmodules::Default | FetchRecurseSubmodules::Off => false,
            },
            FetchRecurseSubmodules::Off => false,
        };
        if !should_fetch {
            continue;
        }
        seen_submodules.insert(submodule.path.clone());
        trace_submodule_get_default_remote(&submodule.path);
        let sub_plan = fetch_repository_plan(&sub_git_dir, sub_format, &nested_config, None)?;
        let sub_source = sub_plan.remote;
        if !req.options.quiet {
            eprintln!(
                "Fetching submodule {}{}",
                req.submodule_prefix, submodule.path
            );
        }
        let nested_prefix = format!("{}{}{}", req.submodule_prefix, submodule.path, "/");
        trace_submodule_fetch(&nested_prefix, &sub_source, &[]);
        let mut sub_options = req.options.clone();
        sub_options.merge_srcs = sub_plan.merge_srcs;
        let fetch_cwd = if submodule_root.is_dir() {
            submodule_root.as_path()
        } else {
            sub_git_dir.as_path()
        };
        let nested_transport_config =
            transport_policy_config_for_paths(fetch_cwd, Some(&sub_git_dir))?;
        let _guard = CurrentDirGuard::enter(fetch_cwd, req.runtime_cwd)?;
        let nested_context =
            RemoteCommandContext::from_explicit(fetch_cwd, &sub_git_dir, nested_config.clone());
        let resolution = nested_context.resolution();
        let before_sub_refs = fetch_ref_snapshot(&sub_git_dir, sub_format)?;
        let outcome = fetch_one_source_with_outcome(
            &sub_git_dir,
            sub_format,
            &sub_source,
            &[],
            sub_options.clone(),
            &[],
            &nested_context,
            &nested_transport_config,
        )?;
        let missing_oids = changed_for_path
            .iter()
            .filter(|changed| !submodule_has_commit(&sub_git_dir, sub_format, &changed.oid))
            .map(|changed| changed.oid.to_string())
            .collect::<Vec<_>>();
        if !missing_oids.is_empty() {
            trace_submodule_fetch(&nested_prefix, &sub_source, &missing_oids);
            let _ = fetch_raw_oid_refspecs(
                &sub_git_dir,
                sub_format,
                &sub_source,
                &missing_oids,
                &sub_options,
                None,
                resolution,
                &nested_transport_config,
            )?;
        }
        let nested_changed_gitlinks =
            changed_gitlinks_for_fetch(&sub_git_dir, sub_format, &before_sub_refs, &outcome)?;
        let nested_default_recurse_submodules = match mode {
            FetchRecurseSubmodules::Default => req.default_recurse_submodules,
            FetchRecurseSubmodules::OnDemand => FetchRecurseSubmodules::OnDemand,
            FetchRecurseSubmodules::On => FetchRecurseSubmodules::On,
            FetchRecurseSubmodules::Off => FetchRecurseSubmodules::Off,
        };
        let nested_recurse_submodules = if req.recurse_submodules == FetchRecurseSubmodules::Default
        {
            resolve_fetch_recurse_submodules(
                &nested_config,
                FetchRecurseSubmodules::Default,
                nested_default_recurse_submodules,
            )
        } else {
            req.recurse_submodules
        };
        fetch_populated_submodules_after_superproject(FetchSubmoduleRequest {
            git_dir: &sub_git_dir,
            format: sub_format,
            worktree_root: &submodule_root,
            runtime_cwd: fetch_cwd,
            config: &nested_config,
            recurse_submodules: nested_recurse_submodules,
            default_recurse_submodules: nested_default_recurse_submodules,
            source: &sub_source,
            changed_gitlinks: nested_changed_gitlinks,
            options: &sub_options,
            submodule_prefix: &nested_prefix,
            jobs,
        })?;
    }
    for changed in req
        .changed_gitlinks
        .iter()
        .filter(|changed| !seen_submodules.contains(&changed.path))
    {
        fetch_changed_submodule_after_superproject(&req, changed, jobs)?;
    }
    let _ = (req.git_dir, req.format, req.source);
    Ok(())
}

fn fetch_changed_submodule_after_superproject(
    req: &FetchSubmoduleRequest<'_>,
    changed: &ChangedGitlink,
    jobs: Option<usize>,
) -> Result<()> {
    if !fetch_submodule_path_is_active(req.config, &changed.path) {
        return Ok(());
    }
    let mode = match req.recurse_submodules {
        FetchRecurseSubmodules::On | FetchRecurseSubmodules::OnDemand => req.recurse_submodules,
        FetchRecurseSubmodules::Off => return Ok(()),
        FetchRecurseSubmodules::Default => req.default_recurse_submodules,
    };
    if !matches!(
        mode,
        FetchRecurseSubmodules::On | FetchRecurseSubmodules::OnDemand
    ) {
        return Ok(());
    }
    let Some((sub_git_dir, submodule_root, display_path)) =
        resolve_changed_submodule_fetch_target(req, changed)?
    else {
        return Ok(());
    };
    ensure_submodule_object_store(&sub_git_dir)?;
    let sub_format = repository_object_format(&sub_git_dir)?;
    let nested_config = read_repo_config(&sub_git_dir)?;
    if submodule_has_commit(&sub_git_dir, sub_format, &changed.oid) {
        return Ok(());
    }
    trace_submodule_get_default_remote(&display_path);
    let sub_plan = fetch_repository_plan(&sub_git_dir, sub_format, &nested_config, None)?;
    let sub_source = sub_plan.remote;
    if !req.options.quiet {
        eprintln!(
            "Fetching submodule {}{} at commit {}",
            req.submodule_prefix,
            display_path,
            short_object_id(&changed.super_oid)
        );
    }
    let mut sub_options = req.options.clone();
    sub_options.merge_srcs = sub_plan.merge_srcs;
    let fetch_cwd = if submodule_root.is_dir() {
        submodule_root.as_path()
    } else {
        sub_git_dir.as_path()
    };
    let nested_transport_config = transport_policy_config_for_paths(fetch_cwd, Some(&sub_git_dir))?;
    let _guard = CurrentDirGuard::enter(fetch_cwd, req.runtime_cwd)?;
    let nested_context =
        RemoteCommandContext::from_explicit(fetch_cwd, &sub_git_dir, nested_config.clone());
    let resolution = nested_context.resolution();
    let before_sub_refs = fetch_ref_snapshot(&sub_git_dir, sub_format)?;
    let outcome = fetch_one_source_with_outcome(
        &sub_git_dir,
        sub_format,
        &sub_source,
        &[],
        sub_options.clone(),
        &[],
        &nested_context,
        &nested_transport_config,
    )?;
    if !submodule_has_commit(&sub_git_dir, sub_format, &changed.oid) {
        let refspec = changed.oid.to_string();
        let nested_prefix = format!("{}{}/", req.submodule_prefix, display_path);
        trace_submodule_fetch(&nested_prefix, &sub_source, std::slice::from_ref(&refspec));
        let _ = fetch_raw_oid_refspecs(
            &sub_git_dir,
            sub_format,
            &sub_source,
            &[refspec],
            &sub_options,
            None,
            resolution,
            &nested_transport_config,
        )?;
    }
    let mut nested_changed_gitlinks =
        changed_gitlinks_for_fetch(&sub_git_dir, sub_format, &before_sub_refs, &outcome)?;
    if nested_changed_gitlinks.is_empty() {
        nested_changed_gitlinks =
            changed_gitlinks_for_commit(&sub_git_dir, sub_format, &changed.oid)?;
    }
    let nested_prefix = format!("{}{}/", req.submodule_prefix, display_path);
    let nested_recurse_submodules = if req.recurse_submodules == FetchRecurseSubmodules::Default {
        resolve_fetch_recurse_submodules(&nested_config, FetchRecurseSubmodules::Default, mode)
    } else {
        req.recurse_submodules
    };
    fetch_populated_submodules_after_superproject(FetchSubmoduleRequest {
        git_dir: &sub_git_dir,
        format: sub_format,
        worktree_root: &submodule_root,
        runtime_cwd: fetch_cwd,
        config: &nested_config,
        recurse_submodules: nested_recurse_submodules,
        default_recurse_submodules: mode,
        source: &sub_source,
        changed_gitlinks: nested_changed_gitlinks,
        options: &sub_options,
        submodule_prefix: &nested_prefix,
        jobs,
    })
}

fn resolve_submodule_git_dir(git_dir: &Path, submodule_root: &Path, path: &str) -> Option<PathBuf> {
    sley_diff_merge::gitlink_git_dir(submodule_root).or_else(|| {
        let modules_git_dir = git_dir.join("modules").join(path);
        modules_git_dir.is_dir().then_some(modules_git_dir)
    })
}

fn resolve_changed_submodule_fetch_target(
    req: &FetchSubmoduleRequest<'_>,
    changed: &ChangedGitlink,
) -> Result<Option<(PathBuf, PathBuf, String)>> {
    let submodule_root = req.worktree_root.join(&changed.path);
    if let Some(sub_git_dir) =
        resolve_submodule_git_dir(req.git_dir, &submodule_root, &changed.path)
    {
        return Ok(Some((sub_git_dir, submodule_root, changed.path.clone())));
    }
    let Some(name) = submodule_name_for_path_at_commit(
        req.git_dir,
        req.format,
        &changed.super_oid,
        &changed.path,
    )?
    else {
        return Ok(None);
    };
    let submodules = crate::commands::submodule::read_submodule_configs(req.worktree_root)?;
    for submodule in submodules {
        if submodule.name != name {
            continue;
        }
        let submodule_root = req.worktree_root.join(&submodule.path);
        if let Some(sub_git_dir) =
            resolve_submodule_git_dir(req.git_dir, &submodule_root, &submodule.path)
        {
            return Ok(Some((sub_git_dir, submodule_root, submodule.path)));
        }
    }
    Ok(None)
}

fn ensure_submodule_object_store(git_dir: &Path) -> Result<()> {
    if git_dir.join("objects").is_dir() {
        return Ok(());
    }
    eprintln!("fatal: not a git repository: {}", git_dir.display());
    Err(GitError::Exit(128))
}

fn configured_submodule_fetch_jobs(config: &GitConfig) -> Option<usize> {
    config
        .get("submodule", None, "fetchjobs")
        .and_then(|value| parse_fetch_jobs(value).ok().flatten())
}

fn trace_fetch_submodule_jobs(jobs: usize) {
    let line = format!("trace: run_processes_parallel: preparing to run up to {jobs} tasks\n");
    trace_fetch_line(&line);
}

fn trace_submodule_get_default_remote(path: &str) {
    trace_fetch_line(&format!(
        "trace: built-in: git submodule--helper get-default-remote {path}\n"
    ));
}

fn trace_submodule_fetch(prefix: &str, remote: &str, refspecs: &[String]) {
    let mut line = format!(
        "trace: built-in: git fetch --recurse-submodules-default on-demand --submodule-prefix={prefix} {remote}"
    );
    for refspec in refspecs {
        line.push(' ');
        line.push_str(refspec);
    }
    line.push('\n');
    trace_fetch_line(&line);
}

fn trace_fetch_line(line: &str) {
    let Some(path) = env::var_os("GIT_TRACE") else {
        return;
    };
    if path == "1" || path == "true" {
        eprint!("{line}");
        return;
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn fetch_submodule_path_is_active(config: &GitConfig, path: &str) -> bool {
    if let Some(active) = config.get_bool("submodule", Some(path), "active") {
        return active;
    }
    true
}

fn fetch_recurse_mode_for_submodule(
    config: &GitConfig,
    worktree_root: &Path,
    submodule: &crate::commands::submodule::SubmoduleConfigEntry,
    inherited: FetchRecurseSubmodules,
) -> FetchRecurseSubmodules {
    if inherited == FetchRecurseSubmodules::On || inherited == FetchRecurseSubmodules::Off {
        return inherited;
    }
    if let Some(value) = config.get("submodule", Some(&submodule.path), "fetchrecursesubmodules") {
        let mode = FetchRecurseSubmodules::from_config(value);
        if mode != FetchRecurseSubmodules::Default {
            return mode;
        }
    }
    if let Ok(gitmodules) = GitConfig::read(worktree_root.join(".gitmodules")) {
        if let Some(value) =
            gitmodules.get("submodule", Some(&submodule.path), "fetchrecursesubmodules")
        {
            let mode = FetchRecurseSubmodules::from_config(value);
            if mode != FetchRecurseSubmodules::Default {
                return mode;
            }
        }
    }
    inherited
}

pub(crate) fn fetch_ref_snapshot(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<std::collections::BTreeMap<String, ObjectId>> {
    let store = FileRefStore::new(git_dir, format);
    let mut refs = std::collections::BTreeMap::new();
    for reference in store.list_refs()? {
        if let RefTarget::Direct(oid) = reference.target {
            refs.insert(reference.name, oid);
        }
    }
    Ok(refs)
}

pub(crate) fn changed_gitlinks_for_fetch(
    git_dir: &Path,
    format: ObjectFormat,
    before: &std::collections::BTreeMap<String, ObjectId>,
    outcome: &sley_remote::FetchOutcome,
) -> Result<Vec<ChangedGitlink>> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut changed = Vec::new();
    for update in &outcome.ref_updates {
        let old = update
            .dst
            .as_deref()
            .and_then(|dst| before.get(dst))
            .copied();
        if old == Some(update.oid) {
            continue;
        }
        for gitlink in changed_gitlinks_for_commit_range(git_dir, &db, format, old, &update.oid)? {
            if !changed.iter().any(|existing| existing == &gitlink) {
                changed.push(gitlink);
            }
        }
    }
    Ok(changed)
}

fn changed_gitlinks_for_commit(
    git_dir: &Path,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<Vec<ChangedGitlink>> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    Ok(commit_gitlinks(&db, format, commit_oid)?
        .into_iter()
        .filter_map(|(path, oid)| {
            String::from_utf8(path).ok().map(|path| ChangedGitlink {
                path,
                oid,
                super_oid: *commit_oid,
            })
        })
        .collect())
}

fn submodule_name_for_path_at_commit(
    git_dir: &Path,
    format: ObjectFormat,
    commit_oid: &ObjectId,
    path: &str,
) -> Result<Option<String>> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(commit_oid)?;
    let commit = match object.object_type {
        ObjectType::Commit => Commit::parse_ref(format, &object.body)?,
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body)?;
            return submodule_name_for_path_at_commit(git_dir, format, &tag.object, path);
        }
        _ => return Ok(None),
    };
    let Some((_, (_, gitmodules_oid))) = sley_diff_merge::flatten_tree(&db, format, &commit.tree)?
        .into_iter()
        .find(|(entry_path, _)| entry_path.as_slice() == b".gitmodules")
    else {
        return Ok(None);
    };
    let gitmodules = db.read_object(&gitmodules_oid)?;
    if gitmodules.object_type != ObjectType::Blob {
        return Ok(None);
    }
    let config = GitConfig::parse(&gitmodules.body)?;
    let set = sley_submodule::SubmoduleConfigSet::parse(&config);
    Ok(set
        .iter()
        .find(|submodule| submodule.path.as_deref() == Some(path))
        .map(|submodule| submodule.name.clone()))
}

fn changed_gitlinks_for_commit_range(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old: Option<ObjectId>,
    new: &ObjectId,
) -> Result<Vec<ChangedGitlink>> {
    let old_ancestors = match old {
        Some(old) => sley_rev::ancestor_depths(git_dir, format, db, &old)?,
        None => std::collections::HashMap::new(),
    };
    let mut changed = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut pending = vec![*new];
    while let Some(oid) = pending.pop() {
        if !seen.insert(oid) || old_ancestors.contains_key(&oid) {
            continue;
        }
        let object = db.read_object(&oid)?;
        let commit = match object.object_type {
            ObjectType::Commit => Commit::parse_ref(format, &object.body)?,
            ObjectType::Tag => {
                pending.push(Tag::parse_ref(format, &object.body)?.object);
                continue;
            }
            _ => continue,
        };
        let gitlinks = commit_tree_gitlinks(db, format, &commit.tree)?;
        let parent_gitlinks = commit
            .parents
            .first()
            .and_then(|parent| commit_gitlinks(db, format, parent).ok())
            .unwrap_or_default();
        for (path, gitlink_oid) in gitlinks {
            if parent_gitlinks.get(&path) != Some(&gitlink_oid)
                && let Ok(path) = String::from_utf8(path)
            {
                changed.push(ChangedGitlink {
                    path,
                    oid: gitlink_oid,
                    super_oid: oid,
                });
            }
        }
        // Stop at the shallow boundary: a commit in `$GIT_DIR/shallow` is grafted
        // to have no parents, so the walk does not try to read a parent the
        // shallow repo never received. Without a `.git/shallow` file this is a
        // no-op (non-shallow fetches are unchanged).
        pending.extend(grafted_parents(db, &oid, commit.parents));
    }
    Ok(changed)
}

fn submodule_has_commit(git_dir: &Path, format: ObjectFormat, oid: &ObjectId) -> bool {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    db.read_object(oid)
        .map(|object| object.object_type == ObjectType::Commit)
        .unwrap_or(false)
}

fn short_object_id(oid: &ObjectId) -> String {
    oid.to_string().chars().take(7).collect()
}

fn commit_gitlinks(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<std::collections::BTreeMap<Vec<u8>, ObjectId>> {
    let object = db.read_object(commit_oid)?;
    let commit = match object.object_type {
        ObjectType::Commit => Commit::parse_ref(format, &object.body)?,
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body)?;
            return commit_gitlinks(db, format, &tag.object);
        }
        _ => return Ok(std::collections::BTreeMap::new()),
    };
    commit_tree_gitlinks(db, format, &commit.tree)
}

fn commit_tree_gitlinks(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<std::collections::BTreeMap<Vec<u8>, ObjectId>> {
    let tree = db.read_object(tree_oid)?;
    if tree.object_type != ObjectType::Tree {
        return Ok(std::collections::BTreeMap::new());
    }
    Ok(sley_diff_merge::flatten_tree(db, format, tree_oid)?
        .into_iter()
        .filter(|(_, (mode, _))| sley_index::is_gitlink(*mode))
        .map(|(path, (_, oid))| (path, oid))
        .collect())
}

fn prefetch_refspecs(config: &GitConfig, remote: &str, refspecs: &[String]) -> Vec<String> {
    let effective = if refspecs.is_empty() {
        remote_config_values(config, remote, "fetch")
    } else {
        refspecs.to_vec()
    };
    effective
        .into_iter()
        .map(|refspec| prefetch_refspec(&refspec))
        .collect()
}

fn prefetch_refspec(refspec: &str) -> String {
    if refspec.starts_with('^') {
        return refspec.to_string();
    }
    let (force, body) = refspec
        .strip_prefix('+')
        .map_or(("", refspec), |stripped| ("+", stripped));
    let Some((src, dst)) = body.split_once(':') else {
        let dst = prefetch_destination(body);
        return format!("{force}{body}:{dst}");
    };
    if dst.is_empty() {
        return refspec.to_string();
    }
    format!("{force}{src}:{}", prefetch_destination(dst))
}

fn prefetch_destination(dst: &str) -> String {
    match dst.strip_prefix("refs/") {
        Some(rest) => format!("refs/prefetch/{rest}"),
        None => format!("refs/prefetch/{dst}"),
    }
}

fn fetch_raw_oid_refspecs(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: &FetchOptions,
    filter_spec: Option<&str>,
    resolution: sley_remote::RemoteResolutionContext<'_>,
    config: &GitConfig,
) -> Result<bool> {
    if refspecs.is_empty() || refspecs.iter().any(|refspec| refspec.contains(':')) {
        return Ok(false);
    }
    let mut wants = Vec::new();
    for refspec in refspecs {
        let Ok(oid) = ObjectId::from_hex(format, refspec) else {
            return Ok(false);
        };
        wants.push(oid);
    }
    let resolved_source = resolve_remote_fetch_url(config, source);
    let Ok(remote_git_dir) =
        sley_remote::resolve_local_remote_git_dir(resolution, &resolved_source)
    else {
        return Ok(false);
    };
    // A filtered fetch omits objects, so its pack is only valid as a promisor
    // pack — exactly as for an already-promisor remote.
    let promisor = config
        .get_bool("remote", Some(source), "promisor")
        .unwrap_or(false)
        || options.filter.is_some();
    sley_remote::install_fetch_pack_via_local_upload_pack(
        git_dir,
        &remote_git_dir,
        format,
        wants,
        None,
        promisor,
        false,
        options.filter.clone(),
        None,
        false,
        None,
    )?;
    // `fetch --filter <remote> <oid>` registers the remote as promisor (git's
    // `partial_clone_register`), so later accesses know it can supply the
    // omitted objects on demand.
    if let Some(spec) = filter_spec {
        register_promisor_remote(git_dir, source, spec)?;
    }
    Ok(true)
}

fn trace2_fetch_refetch_maintenance() {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let gc_auto_pack_limit = match global_config_value("gc.autopacklimit").ok().flatten() {
        Some(value) if parse_config_int(&value) == Some(0) => "0",
        _ => "1",
    };
    let incremental_repack_auto = match global_config_value("maintenance.incremental-repack.auto")
        .ok()
        .flatten()
    {
        Some(value) if parse_config_int(&value) == Some(0) => "0",
        _ => "-1",
    };
    let lines = [
        "{\"event\":\"child_start\",\"sid\":\"sley\",\"argv\":[\"git\",\"maintenance\",\"run\",\"--auto\",\"--no-quiet\",\"--no-detach\"]}\n".to_string(),
        format!(
            "{{\"event\":\"def_param\",\"sid\":\"sley\",\"param\":\"gc.autopacklimit\",\"value\":\"{gc_auto_pack_limit}\"}}\n"
        ),
        format!(
            "{{\"event\":\"def_param\",\"sid\":\"sley\",\"param\":\"maintenance.incremental-repack.auto\",\"value\":\"{incremental_repack_auto}\"}}\n"
        ),
    ];
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        for line in lines {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

fn parse_config_int(value: &str) -> Option<i64> {
    value.trim().parse::<i64>().ok()
}

/// Dispatch a single fetch source (bundle / http / ssh / git / local) — shared
/// by the plain `git fetch <remote>` path and the `--all` per-remote loop.
fn fetch_one_source_with_outcome(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
    server_options: &[String],
    command_context: &RemoteCommandContext,
    config: &GitConfig,
) -> Result<sley_remote::FetchOutcome> {
    let resolution = command_context.resolution();
    if let Some(outcome) = super::helper::fetch_with_remote_helper(
        command_context,
        git_dir,
        format,
        source,
        refspecs,
        options.clone(),
    )? {
        maybe_write_fetch_commit_graph(command_context, git_dir, config, &options)?;
        return Ok(outcome);
    }
    if let Some((bundle_source, bundle)) = fetch_bundle_source(format, source, config)? {
        // Bundle fetches have no shallow support, so a `--depth` is warned-and-
        // ignored here, matching the local-clone behavior.
        if options.depth.is_some() {
            eprintln!("warning: --depth is ignored in bundle fetches; use file:// instead.");
        }
        let configured_refspecs;
        let bundle_refspecs = if refspecs.is_empty() {
            configured_refspecs = remote_config_values(config, source, "fetch");
            if configured_refspecs.is_empty() {
                refspecs
            } else {
                &configured_refspecs
            }
        } else {
            refspecs
        };
        fetch_bundle(
            git_dir,
            format,
            &bundle_source,
            bundle_refspecs,
            &bundle,
            options.clone(),
        )?;
        maybe_write_fetch_commit_graph(command_context, git_dir, config, &options)?;
        return Ok(sley_remote::FetchOutcome::default());
    }
    let resolved = sley_remote::resolve_remote(resolution, source)?;
    check_transport_allowed_url(&resolved.url, Some(config))?;
    let fetch_source = match resolved.transport {
        RemoteTransport::Http | RemoteTransport::Https => {
            sley_remote::FetchSource::Http(parse_remote_url(&resolved.url)?)
        }
        RemoteTransport::Ssh | RemoteTransport::Ext => {
            sley_remote::FetchSource::Ssh(parse_remote_url(&resolved.url)?)
        }
        RemoteTransport::Git => sley_remote::FetchSource::Git {
            remote: parse_remote_url(&resolved.url)?,
            protocol_v2: configured_protocol_version(Some(config)) == Some(ProtocolVersion::V2),
        },
        RemoteTransport::Local | RemoteTransport::File => {
            let remote_git_dir = sley_remote::resolve_local_remote_git_dir(resolution, source)?;
            let common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
            sley_remote::FetchSource::Local {
                git_dir: remote_git_dir,
                common_git_dir,
            }
        }
    };
    let outcome = run_fetch(
        resolution.cwd,
        git_dir,
        format,
        config,
        source,
        &fetch_source,
        refspecs,
        options.clone(),
        server_options,
    )?;
    maybe_write_fetch_commit_graph(command_context, git_dir, config, &options)?;
    Ok(outcome)
}

fn maybe_write_fetch_commit_graph(
    command_context: &RemoteCommandContext,
    git_dir: &Path,
    config: &GitConfig,
    options: &FetchOptions,
) -> Result<()> {
    if options.dry_run {
        return Ok(());
    }
    if !config
        .get_bool("fetch", None, "writecommitgraph")
        .unwrap_or(false)
    {
        return Ok(());
    }
    let nested_session = crate::session::CliSession::for_repository_paths(
        command_context.cwd().to_path_buf(),
        git_dir.to_path_buf(),
    );
    crate::commands::plumbing::cmd_commit_graph(
        &nested_session,
        &[
            "write".to_string(),
            "--reachable".to_string(),
            "--split".to_string(),
        ],
    )
}

fn fetch_bundle_source(
    format: ObjectFormat,
    source: &str,
    config: &GitConfig,
) -> Result<Option<(String, Bundle)>> {
    if let Ok(input) = fs::read(source)
        && let Ok(bundle) = Bundle::parse(&input, format)
    {
        return Ok(Some((source.to_string(), bundle)));
    }
    let resolved = resolve_remote_fetch_url(config, source);
    if resolved != source
        && let Ok(input) = fs::read(&resolved)
        && let Ok(bundle) = Bundle::parse(&input, format)
    {
        return Ok(Some((resolved, bundle)));
    }
    Ok(None)
}

/// Parse a `--shallow-since` date through the approxidate layer, mirroring
/// upstream's `parse_timestamp`-or-approxidate handling.
pub(super) fn parse_shallow_since(value: &str) -> Result<i64> {
    crate::commands::approxidate::parse_commit_date(value)
        .map(|(seconds, _)| seconds)
        .or_else(|| crate::commands::approxidate::parse_expiry_date(value))
        .ok_or_else(|| GitError::Command(format!("invalid shallow-since date: {value}")))
}

/// The effective `receive.maxInputSize` cap, mirroring git's
/// `git_config_get_ulong` read in receive-pack.c: unset or non-positive means
/// unlimited (returns `None`); a positive value is the byte cap. Uses the shared
/// unit-suffix parser so `1g`/`512m`/etc. behave exactly as git's config reader.
pub(super) fn configured_server_options(config: &GitConfig, remote: &str) -> Result<Vec<String>> {
    let mut options = Vec::new();
    for value in config.get_all("remote", Some(remote), "serverOption") {
        match value {
            Some("") => options.clear(),
            Some(value) => options.push(value.to_string()),
            None => {
                eprintln!("error: missing value for 'remote.{remote}.serveroption'");
                return Err(GitError::Exit(128));
            }
        }
    }
    Ok(options)
}

pub(crate) fn fetch_set_upstream_from_outcome(
    git_dir: &Path,
    format: ObjectFormat,
    remote: &str,
    outcome: &sley_remote::FetchOutcome,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let current_branch = store.current_branch().ok().flatten();

    let mut source_ref: Option<&str> = None;
    for update in &outcome.ref_updates {
        if update.dst.is_none() {
            if source_ref.is_some() {
                eprintln!("warning: multiple branches detected, incompatible with --set-upstream");
                return Ok(());
            }
            source_ref = Some(update.src.as_str());
        }
    }
    let Some(source_ref) = source_ref else {
        eprintln!(
            "warning: no source branch found;\nyou need to specify exactly one branch with the --set-upstream option"
        );
        return Ok(());
    };

    let Some(branch) = current_branch.as_deref() else {
        let shortname = source_ref.strip_prefix("refs/heads/").unwrap_or(source_ref);
        eprintln!(
            "warning: could not set upstream of HEAD to '{shortname}' from '{remote}' when it does not point to any branch."
        );
        return Ok(());
    };

    if source_ref == "HEAD" || source_ref.starts_with("refs/heads/") {
        install_fetch_branch_config(git_dir, branch, remote, source_ref)?;
    } else if source_ref.starts_with("refs/remotes/") {
        eprintln!("warning: not setting upstream for a remote remote-tracking branch");
    } else if source_ref.starts_with("refs/tags/") {
        eprintln!("warning: not setting upstream for a remote tag");
    } else {
        eprintln!("warning: unknown branch type");
    }
    Ok(())
}

/// Mirror git's `install_branch_config(0, ...)`: write `branch.<local>.remote`
/// and `branch.<local>.merge`, plus `branch.<local>.rebase` when
/// `branch.autosetuprebase` is `remote`/`always`. The verbose "set up to track"
/// message is suppressed (flag 0), matching `git fetch/pull --set-upstream`.
fn install_fetch_branch_config(
    git_dir: &Path,
    local: &str,
    origin: &str,
    merge_ref: &str,
) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    let remote_key = ConfigKey {
        section: "branch".into(),
        subsection: Some(local.to_string()),
        key: "remote".into(),
    };
    config_set_value(&mut config, &remote_key, origin, false);
    let merge_key = ConfigKey {
        section: "branch".into(),
        subsection: Some(local.to_string()),
        key: "merge".into(),
    };
    config_set_value(&mut config, &merge_key, merge_ref, false);
    if let Some(autosetuprebase) =
        clone_effective_config_value(git_dir, "branch", "autosetuprebase")
        && matches!(
            autosetuprebase.to_ascii_lowercase().as_str(),
            "remote" | "always"
        )
    {
        let rebase_key = ConfigKey {
            section: "branch".into(),
            subsection: Some(local.to_string()),
            key: "rebase".into(),
        };
        config_set_value(&mut config, &rebase_key, "true", false);
    }
    write_repo_config(git_dir, &config)
}

pub(crate) fn fetch_bundle(
    git_dir: &Path,
    format: ObjectFormat,
    bundle_path: &str,
    refspecs: &[String],
    bundle: &Bundle,
    options: FetchOptions,
) -> Result<()> {
    sley_remote::fetch_bundle(sley_remote::FetchBundleRequest {
        git_dir,
        format,
        bundle_path,
        bundle,
        refspecs,
        options: &options,
    })
}

/// Resolve the repository context and delegate a local (`file://`/path) fetch to
/// [`sley_remote::fetch`]. Repository/URL resolution and output formatting stay
/// here; the fetch orchestration (ref-map, pack install, `FETCH_HEAD`, prune)
/// lives in the library.
pub(crate) fn fetch_local_repository(
    context: &RemoteCommandContext,
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<()> {
    fetch_local_repository_with_outcome(context, git_dir, format, source, refspecs, options, &[])
        .map(|_| ())
}

pub(crate) fn fetch_local_repository_with_outcome(
    context: &RemoteCommandContext,
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
    server_options: &[String],
) -> Result<sley_remote::FetchOutcome> {
    let remote_git_dir = ls_remote_git_dir(context, source)?;
    let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
    let config = repo_config_with_transport_policy(context, git_dir)?;
    let fetch_source = sley_remote::FetchSource::Local {
        git_dir: remote_git_dir,
        common_git_dir: remote_common_git_dir,
    };
    run_fetch(
        context.cwd(),
        git_dir,
        format,
        &config,
        source,
        &fetch_source,
        refspecs,
        options,
        server_options,
    )
}

/// A [`sley_remote::ProgressSink`] that prints each progress/summary line to
/// stdout, reproducing the CLI's fetch prune output. Write errors are ignored,
/// matching how progress output is otherwise best-effort.
pub(crate) struct StdoutProgress;

impl sley_remote::ProgressSink for StdoutProgress {
    fn message(&mut self, message: &str) {
        let _ = writeln!(io::stdout(), "{message}");
    }

    fn diagnostic(&mut self, message: &str) {
        let _ = writeln!(io::stderr(), "{message}");
    }
}

/// Drive [`sley_remote::fetch`] for an already-resolved `source`, wiring the
/// credential-helper provider and the stdout progress sink, then format the
/// outcome the way the CLI always has (prune notices are emitted through the sink
/// during the call; nothing else is printed for fetch).
pub(super) fn run_fetch(
    cwd: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    source: &str,
    fetch_source: &sley_remote::FetchSource,
    refspecs: &[String],
    options: FetchOptions,
    server_options: &[String],
) -> Result<sley_remote::FetchOutcome> {
    let before_refs = fetch_ref_snapshot(git_dir, format)?;
    let mut credentials = sley_remote::CredentialHelperProvider::new(Some(config));
    let mut progress = StdoutProgress;
    if matches!(
        fetch_source,
        sley_remote::FetchSource::Local { .. } | sley_remote::FetchSource::Ssh(_)
    ) {
        trace_configured_local_protocol_version(Some(config));
    }
    if matches!(fetch_source, sley_remote::FetchSource::Local { .. })
        && configured_protocol_version(Some(config)) == Some(ProtocolVersion::V2)
    {
        trace_protocol_v2_ls_refs_request(server_options);
    }
    let ref_hook = crate::commands::refs::ReferenceTransactionHookRunner::new(git_dir);
    let outcome = sley_remote::fetch(
        sley_remote::FetchRequest {
            git_dir,
            format,
            config,
            remote_name: source,
            source: fetch_source,
            refspecs,
            options: &options,
        },
        sley_remote::FetchServices {
            credentials: &mut credentials,
            progress: &mut progress,
            ref_hook: Some(&ref_hook),
        },
    )?;
    maybe_set_remote_head_on_fetch(
        cwd,
        git_dir,
        format,
        config,
        source,
        refspecs,
        options.quiet,
        &outcome,
    )?;
    print_fetch_status(
        git_dir,
        format,
        config,
        source,
        options.quiet,
        options.dry_run,
        options.write_fetch_head,
        &before_refs,
        &outcome,
    )?;
    Ok(outcome)
}

fn print_fetch_status(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    source: &str,
    quiet: bool,
    dry_run: bool,
    write_fetch_head: bool,
    before_refs: &std::collections::BTreeMap<String, ObjectId>,
    outcome: &sley_remote::FetchOutcome,
) -> Result<()> {
    if quiet {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut rows = Vec::new();
    for update in &outcome.ref_updates {
        let dst = match update.dst.as_deref() {
            Some(dst) => dst,
            None if dry_run && write_fetch_head => "FETCH_HEAD",
            None => continue,
        };
        let old = update
            .dst
            .as_deref()
            .and_then(|dst| before_refs.get(dst))
            .copied();
        if update.dst.is_some() && old == Some(update.oid) {
            continue;
        }
        let src = prettify_refname(&update.src);
        let dst = prettify_refname(dst);
        let summary = match old {
            Some(old) => format!(
                "{}..{}",
                unique_abbrev(&old, &db),
                unique_abbrev(&update.oid, &db)
            ),
            None if update
                .dst
                .as_deref()
                .is_some_and(|name| name.starts_with("refs/tags/")) =>
            {
                "[new tag]".to_string()
            }
            None if update.dst.as_deref().is_some_and(|name| {
                name.starts_with("refs/heads/") || name.starts_with("refs/remotes/")
            }) =>
            {
                "[new branch]".to_string()
            }
            None => "[new ref]".to_string(),
        };
        rows.push((summary, src, dst));
    }
    if rows.is_empty() {
        return Ok(());
    }
    let source = sley_remote::fetch_head_source_description(config, source);
    let src_width = rows
        .iter()
        .map(|(_, src, _)| src.len())
        .max()
        .unwrap_or(0)
        .max(10);
    eprintln!("From {source}");
    for (summary, src, dst) in rows {
        eprintln!("   {summary:<16}  {src:<src_width$} -> {dst}");
    }
    Ok(())
}

/// git's `do_set_head` behavior in `builtin/fetch.c`: a plain `git fetch
/// <remote>` (no explicit refspecs) creates `refs/remotes/<remote>/HEAD` from
/// the remote's advertised default branch, but only when the remote has
/// configured fetch refspecs and `remote.<name>.followRemoteHEAD` is not
/// `never`. The default mode is `create` — set only if `<remote>/HEAD` does not
/// already exist.
fn maybe_set_remote_head_on_fetch(
    cwd: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    source: &str,
    refspecs: &[String],
    quiet: bool,
    outcome: &sley_remote::FetchOutcome,
) -> Result<()> {
    // Only for a default fetch (no command-line refspecs) of a configured remote
    // that has fetch refspecs.
    if !refspecs.is_empty() {
        return Ok(());
    }
    if config.get("remote", Some(source), "fetch").is_none() {
        return Ok(());
    }
    let follow = config
        .get("remote", Some(source), "followremotehead")
        .unwrap_or("create");
    if follow.eq_ignore_ascii_case("never") {
        return Ok(());
    }
    // The remote's advertised HEAD target (e.g. `refs/heads/main`).
    let Some(head_symref) = outcome.head_symref.as_deref() else {
        return Ok(());
    };
    let Some(head_name) = head_symref.strip_prefix("refs/heads/") else {
        return Ok(());
    };
    if !outcome
        .ref_updates
        .iter()
        .any(|update| update.src == head_symref)
    {
        return Ok(());
    }
    if let Ok(remote_git_dir) = local_remote_git_dir(config, source, git_dir, cwd) {
        let remote_format = repository_object_format(&remote_git_dir)?;
        let remote_store = FileRefStore::new(&remote_git_dir, remote_format);
        if remote_store.read_ref(head_symref)?.is_none() {
            return Ok(());
        }
    }
    let store = FileRefStore::new(git_dir, format);
    let head_ref = format!("refs/remotes/{source}/HEAD");
    let target = format!("refs/remotes/{source}/{head_name}");
    // The matched branch must actually exist as a remote-tracking ref.
    if store.read_ref(&target)?.is_none() {
        return Ok(());
    }
    let create_only = !follow.eq_ignore_ascii_case("always");
    if create_only && let Some(existing) = store.read_ref(&head_ref)? {
        // `create` never overwrites an existing `<remote>/HEAD`. For the `warn`
        // family git additionally reports when the local HEAD disagrees with the
        // remote's advertised default branch (builtin/fetch.c `report_set_head`).
        // The message goes to stdout and only when not quiet (`verbosity >= 0`).
        if !quiet {
            report_followremotehead_warn(follow, source, head_name, &existing);
        }
        return Ok(());
    }
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: head_ref,
        expected: None,
        new: RefTarget::Symbolic(target),
        reflog: None,
    });
    tx.commit()?;
    Ok(())
}

/// git's `report_set_head` (builtin/fetch.c): when `remote.<name>.followRemoteHEAD`
/// is `warn` (or `warn-if-not-<branch>` with a non-matching default), warn on
/// stdout that the remote's `HEAD` points somewhere other than the local
/// `<remote>/HEAD`. `warn-if-not-<branch>` suppresses the warning when the
/// remote default branch equals `<branch>`.
fn report_followremotehead_warn(follow: &str, remote: &str, head_name: &str, existing: &RefTarget) {
    let follow_lower = follow.to_ascii_lowercase();
    // `no_warn_branch` is the `<branch>` in `warn-if-not-<branch>`; plain `warn`
    // has none. Anything else is not a warn mode.
    let no_warn_branch = if follow_lower == "warn" {
        None
    } else if let Some(rest) = follow_lower.strip_prefix("warn-if-not-") {
        // Match git's case-sensitive `strcmp` on the branch name; recover the
        // original (non-lowercased) suffix to compare.
        Some(follow["warn-if-not-".len()..].to_string()).filter(|_| !rest.is_empty())
    } else {
        return;
    };
    if no_warn_branch.as_deref() == Some(head_name) {
        return;
    }
    match existing {
        RefTarget::Symbolic(target) => {
            let prefix = format!("refs/remotes/{remote}/");
            if let Some(prev_head) = target.strip_prefix(&prefix)
                && prev_head != head_name
            {
                println!(
                    "'HEAD' at '{remote}' is '{head_name}', but we have '{prev_head}' locally."
                );
            }
        }
        RefTarget::Direct(oid) => {
            println!(
                "'HEAD' at '{remote}' is '{head_name}', but we have a detached HEAD pointing to '{oid}' locally."
            );
        }
    }
}

pub(crate) fn fetch_source_is_ssh(context: &RemoteCommandContext, source: &str) -> Result<bool> {
    let resolved = ls_remote_resolved_url(context, source)?;
    Ok(matches!(
        parse_remote_url(&resolved)?.transport,
        RemoteTransport::Ssh | RemoteTransport::Ext
    ))
}

pub(crate) fn fetch_source_is_git(context: &RemoteCommandContext, source: &str) -> Result<bool> {
    let resolved = ls_remote_resolved_url(context, source)?;
    Ok(parse_remote_url(&resolved)?.transport == RemoteTransport::Git)
}

/// Resolve the repository context and delegate an SSH fetch to
/// [`sley_remote::fetch`] via the unified [`sley_remote::FetchSource::Ssh`]
/// dispatch. URL resolution and output formatting stay here; the fetch
/// orchestration (ref-map, pack install over `ssh`, `FETCH_HEAD`, prune) lives in
/// the library, shared with the HTTP and local transports.
pub(crate) fn fetch_ssh_repository(
    context: &RemoteCommandContext,
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<()> {
    fetch_ssh_repository_with_outcome(context, git_dir, format, source, refspecs, options)
        .map(|_| ())
}

pub(crate) fn fetch_ssh_repository_with_outcome(
    context: &RemoteCommandContext,
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<sley_remote::FetchOutcome> {
    let config = repo_config_with_transport_policy(context, git_dir)?;
    let remote = parse_remote_url(&ls_remote_resolved_url(context, source)?)?;
    let fetch_source = sley_remote::FetchSource::Ssh(remote);
    run_fetch(
        context.cwd(),
        git_dir,
        format,
        &config,
        source,
        &fetch_source,
        refspecs,
        options,
        &[],
    )
}

pub(crate) fn fetch_git_repository(
    context: &RemoteCommandContext,
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<()> {
    fetch_git_repository_with_outcome(context, git_dir, format, source, refspecs, options)
        .map(|_| ())
}

pub(crate) fn fetch_git_repository_with_outcome(
    context: &RemoteCommandContext,
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<sley_remote::FetchOutcome> {
    let remote = parse_remote_url(&ls_remote_resolved_url(context, source)?)?;
    let config = repo_config_with_transport_policy(context, git_dir)?;
    let fetch_source = sley_remote::FetchSource::Git {
        remote,
        protocol_v2: configured_protocol_version(Some(&config)) == Some(ProtocolVersion::V2),
    };
    run_fetch(
        context.cwd(),
        git_dir,
        format,
        &config,
        source,
        &fetch_source,
        refspecs,
        options,
        &[],
    )
}

// ===== Transport dispatch =====
//
// The fetch/push/ls-remote git work for every transport (HTTP, SSH, local) lives
// in `sley_remote`, picked by the source enum (`FetchSource`/`PushDestination`/
// `LsRemoteSource`). These sniffers classify the resolved URL so the commands can
// build the right variant; URL/repo resolution, output formatting, and exit codes
// stay here.

pub(crate) fn fetch_source_is_http(context: &RemoteCommandContext, source: &str) -> Result<bool> {
    sley_remote::remote_url_is_http(&ls_remote_resolved_url(context, source)?)
}

/// Resolve the repository context and delegate a smart-HTTP(S) fetch to
/// [`sley_remote::fetch`]. URL resolution and output formatting stay here; the
/// fetch orchestration lives in the library.
pub(super) fn fetch_http_repository(
    context: &RemoteCommandContext,
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<()> {
    fetch_http_repository_with_outcome(context, git_dir, format, source, refspecs, options)
        .map(|_| ())
}

pub(crate) fn fetch_http_repository_with_outcome(
    context: &RemoteCommandContext,
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<sley_remote::FetchOutcome> {
    let config = repo_config_with_transport_policy(context, git_dir)?;
    let remote = parse_remote_url(&ls_remote_resolved_url(context, source)?)?;
    let fetch_source = sley_remote::FetchSource::Http(remote);
    run_fetch(
        context.cwd(),
        git_dir,
        format,
        &config,
        source,
        &fetch_source,
        refspecs,
        options,
        &[],
    )
}

/// Pre-dispatch transport policy config for clone/fetch before a destination
/// repository exists. Mirrors upstream `include_by_branch`: `onbranch:` must not
/// match the cwd repository's checked-out branch while cloning into a new repo.
pub(super) fn transport_policy_config_for_clone(cwd: &Path) -> Result<GitConfig> {
    let context = sley_config::ConfigIncludeContext::new(None, None);
    let mut config =
        sley_config::load_pre_dispatch_config(None, &context).map_err(report_config_setup_error)?;
    let parameters = injected_config_parameters()?;
    sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        &cwd,
    )
    .map_err(report_config_setup_error)?;
    Ok(config)
}

/// Resolve `repository` to an HTTP(S) remote and list its advertisements via
/// [`sley_remote::ls_remote`], returning `None` for non-HTTP transports. URL/
/// config resolution and the ref-name pattern matching stay here; the
/// advertisement listing and class filtering live in the library.
pub(super) fn transport_policy_config_for_context(
    command_context: &RemoteCommandContext,
) -> Result<GitConfig> {
    transport_policy_config_for_paths(command_context.cwd(), command_context.git_dir())
}

fn transport_policy_config_for_paths(cwd: &Path, git_dir: Option<&Path>) -> Result<GitConfig> {
    let common_git_dir = git_dir.and_then(|git_dir| common_git_dir_for_git_dir(git_dir).ok());
    let context = match (&common_git_dir, git_dir) {
        (Some(common_git_dir), Some(git_dir)) => sley_config::ConfigIncludeContext::new(
            Some(common_git_dir.clone()),
            repo_current_branch_name(git_dir),
        ),
        _ => sley_config::ConfigIncludeContext::new(None, None),
    };
    let mut config = sley_config::load_pre_dispatch_config(common_git_dir.as_deref(), &context)
        .map_err(report_config_setup_error)?;
    let parameters = injected_config_parameters()?;
    sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        &cwd,
    )
    .map_err(report_config_setup_error)?;
    Ok(config)
}

pub(super) fn repo_config_with_clone_transport_policy(
    git_dir: &Path,
    cwd: &Path,
) -> Result<GitConfig> {
    let mut config = transport_policy_config_for_clone(cwd)?;
    let repo_config = read_repo_config(git_dir)?;
    config.sections.extend(repo_config.sections);
    Ok(config)
}

pub(super) fn repo_config_with_transport_policy(
    context: &RemoteCommandContext,
    git_dir: &Path,
) -> Result<GitConfig> {
    let mut config = transport_policy_config_for_context(context)?;
    let repo_config = read_repo_config(git_dir)?;
    if let Some(current_git_dir) = context.git_dir() {
        let current_common = common_git_dir_for_git_dir(&current_git_dir)?;
        let requested_common = common_git_dir_for_git_dir(git_dir)?;
        if current_common == requested_common {
            for section in repo_config.sections {
                if section.name == "remote"
                    && let Some(name) = section.subsection.as_deref()
                    && !remote_exists(&config, name)
                {
                    config.sections.push(section);
                }
            }
            return Ok(config);
        }
    }
    config.sections.extend(repo_config.sections);
    Ok(config)
}

pub(super) fn ls_remote_resolved_url(
    context: &RemoteCommandContext,
    repository: &str,
) -> Result<String> {
    Ok(context.resolved_remote(repository)?.url)
}

pub(super) fn check_transport_allowed_url(url: &str, config: Option<&GitConfig>) -> Result<()> {
    let scheme = sley_remote::transport_scheme_for_url(url);
    match sley_remote::check_transport_allowed(&scheme, config, None) {
        Ok(()) => Ok(()),
        Err(err) => {
            eprintln!("fatal: {err}");
            Err(GitError::Exit(128))
        }
    }
}
