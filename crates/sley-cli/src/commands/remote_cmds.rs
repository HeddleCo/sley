//! Remote operations: clone, fetch, push, ls-remote, and `git remote` subcommands.

use crate::commands::config_cmd::{
    ConfigKey, SimpleConfigRegex, config_set_value, parse_config_key,
};
use crate::remote::{
    remote_config_values, resolve_remote_fetch_url, resolve_remote_push_url,
    rewrite_url_with_config,
};
use crate::*;
use sley_remote::{FetchOptions, LsRemoteRecord};

pub(crate) fn cmd_clone(args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut explicit_bare = None::<bool>;
    let mut mirror = false;
    let mut checkout = true;
    let mut tag_opt = None;
    let mut branch = None::<String>;
    let mut revision = None::<String>;
    let mut explicit_single_branch = None::<bool>;
    let mut origin = "origin".to_string();
    let mut config_overrides = Vec::new();
    let mut submodule_active = Vec::<String>::new();
    let mut template = None::<String>;
    let mut template_config = true;
    let mut partial_clone_filter = None::<String>;
    let mut also_filter_submodules = false;
    let mut bundle_uri = None::<String>;
    let mut shared = false;
    let mut reference_alternates = Vec::<CloneReferenceAlternate>::new();
    let mut dissociate = false;
    let mut sparse = false;
    let mut separate_git_dir = None::<String>;
    let mut depth = None::<u32>;
    let mut local = None::<bool>;
    let mut shallow_since_ignored = false;
    let mut shallow_exclude_ignored = false;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-v" | "--verbose" => quiet = false,
            "--no-verbose" => {}
            "--bare" => explicit_bare = Some(true),
            "--no-bare" => explicit_bare = Some(false),
            "--mirror" => mirror = true,
            "--no-mirror" => mirror = false,
            "--checkout" => checkout = true,
            "--no-checkout" | "-n" => checkout = false,
            "--progress" | "--no-progress" => {}
            "--single-branch" => explicit_single_branch = Some(true),
            "--no-single-branch" => explicit_single_branch = Some(false),
            "--tags" => tag_opt = None,
            "--no-tags" => tag_opt = Some("--no-tags".to_string()),
            "--reject-shallow" | "--no-reject-shallow" => {}
            "--depth" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("clone --depth requires a value".into()))?;
                depth = Some(parse_clone_depth(value)?);
            }
            value if value.starts_with("--depth=") => {
                let value = value
                    .strip_prefix("--depth=")
                    .ok_or_else(|| GitError::Command("clone --depth requires a value".into()))?;
                depth = Some(parse_clone_depth(value)?);
            }
            "--no-depth" => depth = None,
            "--shallow-since" => {
                iter.next().ok_or_else(|| {
                    GitError::Command("clone --shallow-since requires a value".into())
                })?;
                shallow_since_ignored = true;
            }
            value if value.starts_with("--shallow-since=") => {
                let _ = value.strip_prefix("--shallow-since=").ok_or_else(|| {
                    GitError::Command("clone --shallow-since requires a value".into())
                })?;
                shallow_since_ignored = true;
            }
            "--no-shallow-since" => shallow_since_ignored = false,
            "--shallow-exclude" => {
                iter.next().ok_or_else(|| {
                    GitError::Command("clone --shallow-exclude requires a value".into())
                })?;
                shallow_exclude_ignored = true;
            }
            value if value.starts_with("--shallow-exclude=") => {
                let _ = value.strip_prefix("--shallow-exclude=").ok_or_else(|| {
                    GitError::Command("clone --shallow-exclude requires a value".into())
                })?;
                shallow_exclude_ignored = true;
            }
            "--no-shallow-exclude" => shallow_exclude_ignored = false,
            "--recurse-submodules" | "--recursive" => submodule_active.push(".".to_string()),
            value if value.starts_with("--recurse-submodules=") => {
                submodule_active.push(
                    value
                        .strip_prefix("--recurse-submodules=")
                        .ok_or_else(|| {
                            GitError::Command("clone --recurse-submodules requires a value".into())
                        })?
                        .to_string(),
                );
            }
            value if value.starts_with("--recursive=") => {
                submodule_active.push(
                    value
                        .strip_prefix("--recursive=")
                        .ok_or_else(|| {
                            GitError::Command("clone --recursive requires a value".into())
                        })?
                        .to_string(),
                );
            }
            "--no-recurse-submodules" | "--no-recursive" => submodule_active.clear(),
            "--also-filter-submodules" => also_filter_submodules = true,
            "--no-also-filter-submodules" => also_filter_submodules = false,
            "--remote-submodules" | "--no-remote-submodules" => {}
            "--shallow-submodules" | "--no-shallow-submodules" => {}
            "--bundle-uri" => {
                bundle_uri = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("clone --bundle-uri requires a value".into())
                        })?
                        .to_string(),
                );
            }
            value if value.starts_with("--bundle-uri=") => {
                bundle_uri = Some(
                    value
                        .strip_prefix("--bundle-uri=")
                        .ok_or_else(|| {
                            GitError::Command("clone --bundle-uri requires a value".into())
                        })?
                        .to_string(),
                );
            }
            "--no-bundle-uri" => bundle_uri = None,
            "--no-jobs" => {}
            "--separate-git-dir" => {
                separate_git_dir = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("clone --separate-git-dir requires a value".into())
                        })?
                        .to_string(),
                );
            }
            value if value.starts_with("--separate-git-dir=") => {
                separate_git_dir = Some(
                    value
                        .strip_prefix("--separate-git-dir=")
                        .ok_or_else(|| {
                            GitError::Command("clone --separate-git-dir requires a value".into())
                        })?
                        .to_string(),
                );
            }
            "--no-separate-git-dir" => separate_git_dir = None,
            "--sparse" => sparse = true,
            "--no-sparse" => sparse = false,
            "-s" | "--shared" => shared = true,
            "--no-shared" => shared = false,
            "--reference" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("clone --reference requires a value".into())
                })?;
                reference_alternates.push(CloneReferenceAlternate {
                    path: value.to_string(),
                    if_able: false,
                });
            }
            value if value.starts_with("--reference=") => {
                reference_alternates.push(CloneReferenceAlternate {
                    path: value
                        .strip_prefix("--reference=")
                        .ok_or_else(|| {
                            GitError::Command("clone --reference requires a value".into())
                        })?
                        .to_string(),
                    if_able: false,
                });
            }
            "--no-reference" => reference_alternates.retain(|reference| reference.if_able),
            "--reference-if-able" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("clone --reference-if-able requires a value".into())
                })?;
                reference_alternates.push(CloneReferenceAlternate {
                    path: value.to_string(),
                    if_able: true,
                });
            }
            value if value.starts_with("--reference-if-able=") => {
                reference_alternates.push(CloneReferenceAlternate {
                    path: value
                        .strip_prefix("--reference-if-able=")
                        .ok_or_else(|| {
                            GitError::Command("clone --reference-if-able requires a value".into())
                        })?
                        .to_string(),
                    if_able: true,
                });
            }
            "--no-reference-if-able" => reference_alternates.retain(|reference| !reference.if_able),
            "--dissociate" => dissociate = true,
            "--no-dissociate" => dissociate = false,
            "--filter" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("clone --filter requires a value".into()))?;
                validate_clone_filter(value)?;
                partial_clone_filter = Some(value.to_string());
            }
            value if value.starts_with("--filter=") => {
                let value = value
                    .strip_prefix("--filter=")
                    .ok_or_else(|| GitError::Command("clone --filter requires a value".into()))?;
                validate_clone_filter(value)?;
                partial_clone_filter = Some(value.to_string());
            }
            "--no-filter" => partial_clone_filter = None,
            "--template" => {
                template = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("clone --template requires a value".into())
                        })?
                        .to_string(),
                );
                template_config = true;
            }
            value if value.starts_with("--template=") => {
                template = Some(
                    value
                        .strip_prefix("--template=")
                        .ok_or_else(|| {
                            GitError::Command("clone --template requires a value".into())
                        })?
                        .to_string(),
                );
                template_config = true;
            }
            "--no-template" => {
                template = None;
                template_config = false;
            }
            "-4" | "--ipv4" | "-6" | "--ipv6" => {}
            "-l" | "--local" => local = Some(true),
            "--no-local" => local = Some(false),
            "--hardlinks" | "--no-hardlinks" => {}
            "--no-ref-format" => {}
            "--ref-format" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("clone --ref-format requires a value".into())
                })?;
                if value != "files" {
                    return Err(GitError::Command(format!(
                        "unsupported clone --ref-format value {value}"
                    )));
                }
            }
            value if value.starts_with("--ref-format=") => {
                let value = value.strip_prefix("--ref-format=").ok_or_else(|| {
                    GitError::Command("clone --ref-format requires a value".into())
                })?;
                if value != "files" {
                    return Err(GitError::Command(format!(
                        "unsupported clone --ref-format value {value}"
                    )));
                }
            }
            "-c" | "--config" => {
                let assignment = iter
                    .next()
                    .ok_or_else(|| GitError::Command("clone --config requires a value".into()))?;
                config_overrides.push(parse_clone_config_override(assignment)?);
            }
            value if value.starts_with("--config=") => {
                config_overrides.push(parse_clone_config_override(
                    value.strip_prefix("--config=").ok_or_else(|| {
                        GitError::Command("clone --config requires a value".into())
                    })?,
                )?);
            }
            "-u" | "--upload-pack" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("clone --upload-pack requires a value".into())
                })?;
                if value.is_empty() {
                    return Err(GitError::Command(
                        "clone --upload-pack requires a value".into(),
                    ));
                }
            }
            value if value.starts_with("--upload-pack=") => {
                let value = value.strip_prefix("--upload-pack=").ok_or_else(|| {
                    GitError::Command("clone --upload-pack requires a value".into())
                })?;
                if value.is_empty() {
                    return Err(GitError::Command(
                        "clone --upload-pack requires a value".into(),
                    ));
                }
            }
            value if value.starts_with("-u") && !value.starts_with("--") && value.len() > 2 => {}
            "--no-upload-pack" => {}
            "--server-option" => {
                iter.next().ok_or_else(|| {
                    GitError::Command("clone --server-option requires a value".into())
                })?;
            }
            value if value.starts_with("--server-option=") => {
                let _ = value.strip_prefix("--server-option=").ok_or_else(|| {
                    GitError::Command("clone --server-option requires a value".into())
                })?;
            }
            "--no-server-option" => {}
            "-j" | "--jobs" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command(clone_jobs_error().into()))?;
                validate_clone_jobs(value)?;
            }
            value if value.starts_with("--jobs=") => {
                validate_clone_jobs(
                    value
                        .strip_prefix("--jobs=")
                        .ok_or_else(|| GitError::Command(clone_jobs_error().into()))?,
                )?;
            }
            value if value.starts_with("-j") && !value.starts_with("--") && value.len() > 2 => {
                validate_clone_jobs(
                    value
                        .strip_prefix("-j")
                        .ok_or_else(|| GitError::Command(clone_jobs_error().into()))?,
                )?;
            }
            "-b" | "--branch" => {
                branch = Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command("clone --branch requires a name".into()))?
                        .to_string(),
                );
            }
            "--no-branch" => branch = None,
            value if value.starts_with("--branch=") => {
                branch = Some(
                    value
                        .strip_prefix("--branch=")
                        .ok_or_else(|| GitError::Command("clone --branch requires a name".into()))?
                        .to_string(),
                );
            }
            value if value.starts_with("-b") && !value.starts_with("--") && value.len() > 2 => {
                branch = Some(
                    value
                        .strip_prefix("-b")
                        .ok_or_else(|| GitError::Command("clone --branch requires a name".into()))?
                        .to_string(),
                );
            }
            "-o" | "--origin" => {
                origin = iter
                    .next()
                    .ok_or_else(|| GitError::Command("clone --origin requires a name".into()))?
                    .to_string();
            }
            "--no-origin" => origin = "origin".to_string(),
            value if value.starts_with("--origin=") => {
                origin = value
                    .strip_prefix("--origin=")
                    .ok_or_else(|| GitError::Command("clone --origin requires a name".into()))?
                    .to_string();
            }
            value if value.starts_with("-o") && !value.starts_with("--") && value.len() > 2 => {
                origin = value
                    .strip_prefix("-o")
                    .ok_or_else(|| GitError::Command("clone --origin requires a name".into()))?
                    .to_string();
            }
            "--revision" => {
                revision = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command("clone --revision requires a value".into())
                        })?
                        .to_string(),
                );
            }
            value if value.starts_with("--revision=") => {
                revision = Some(
                    value
                        .strip_prefix("--revision=")
                        .ok_or_else(|| {
                            GitError::Command("clone --revision requires a value".into())
                        })?
                        .to_string(),
                );
            }
            "--no-revision" => revision = None,
            "--" => {
                positional.extend(iter.map(|value| value.to_string()));
                break;
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported clone option {value}"
                )));
            }
            value => positional.push(value.to_string()),
        }
    }
    if positional.is_empty() || positional.len() > 2 {
        return Err(GitError::Command(
            "clone currently supports: clone [-q|-v] [--bare|--mirror] [--progress|--no-progress] [--single-branch] [--reject-shallow|--no-reject-shallow] [--depth <depth>|--no-depth] [--shallow-since <time>|--no-shallow-since] [--shallow-exclude <revision>|--no-shallow-exclude] [--recurse-submodules|--recursive|--no-recurse-submodules|--no-recursive] [--no-sparse] [--no-filter] [--also-filter-submodules|--no-also-filter-submodules] [--remote-submodules|--no-remote-submodules] [--shallow-submodules|--no-shallow-submodules] [--bundle-uri <uri>|--no-bundle-uri] [-s|--shared|--no-shared] [--reference <repo>|--no-reference] [--reference-if-able <repo>|--no-reference-if-able] [--dissociate|--no-dissociate] [--separate-git-dir <gitdir>|--no-separate-git-dir] [--template <dir>|--no-template|--no-jobs] [--revision <rev>|--no-revision] [-4|--ipv4|-6|--ipv6] [-l|--local|--no-local] [--hardlinks|--no-hardlinks] [--ref-format=files|--no-ref-format] [-c <key=value>] [-u|--upload-pack <path>] [--server-option <value>] [-j|--jobs <n>] [--origin <name>|--no-origin] [--branch <name>|--no-branch] [--tags|--no-tags] <repository> [<directory>]"
                .into(),
        ));
    }
    let single_branch = explicit_single_branch
        .unwrap_or(depth.is_some() || shallow_since_ignored || shallow_exclude_ignored);
    if also_filter_submodules && partial_clone_filter.is_none() {
        eprintln!("fatal: the option '--also-filter-submodules' requires '--filter'");
        return Err(GitError::Exit(128));
    }
    if also_filter_submodules && submodule_active.is_empty() {
        eprintln!("fatal: the option '--also-filter-submodules' requires '--recurse-submodules'");
        return Err(GitError::Exit(128));
    }
    if bundle_uri.is_some() && (depth.is_some() || shallow_since_ignored || shallow_exclude_ignored)
    {
        eprintln!(
            "fatal: options '--bundle-uri' and '--depth/--shallow-since/--shallow-exclude' cannot be used together"
        );
        return Err(GitError::Exit(128));
    }
    if revision.is_some() && branch.is_some() {
        eprintln!("fatal: options '--revision' and '--branch' cannot be used together");
        return Err(GitError::Exit(128));
    }
    validate_remote_name(&origin)?;
    let bare = mirror || explicit_bare.unwrap_or(false);
    if revision.is_some() && mirror {
        eprintln!("fatal: options '--revision' and '--mirror' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if bare && separate_git_dir.is_some() {
        eprintln!("fatal: options '--bare' and '--separate-git-dir' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if bare && sparse {
        eprintln!("fatal: this operation must be run in a work tree");
        eprintln!("error: failed to initialize sparse-checkout");
        return Err(GitError::Exit(128));
    }
    let repository = positional[0].clone();
    let destination = positional
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| default_clone_directory(&repository, bare));
    // git reports the destination as it was given on the command line (or as
    // derived from the source) — `dir` in upstream `builtin/clone.c` — not its
    // absolutized form.
    let destination_display = destination.clone();
    let cwd = env::current_dir()?;
    let destination = if destination.is_absolute() {
        destination
    } else {
        cwd.join(destination)
    };
    let template = template.as_deref().map(|path| resolve_cli_path(&cwd, path));
    let bundle_uri = bundle_uri
        .as_ref()
        .map(|uri| CloneBundleUri::new(&cwd, uri));
    let separate_git_dir = separate_git_dir
        .as_deref()
        .map(|path| resolve_cli_path(&cwd, path));
    if destination.exists() && fs::read_dir(&destination)?.next().is_some() {
        eprintln!(
            "fatal: destination path '{}' already exists and is not an empty directory.",
            destination_display.display()
        );
        return Err(GitError::Exit(128));
    }

    if sley_remote::remote_url_is_http(&repository).unwrap_or(false) {
        return clone_http_repository(CloneHttpOptions {
            repository: &repository,
            destination: &destination,
            destination_display: &destination_display,
            origin: &origin,
            quiet,
            bare,
            checkout,
            sparse,
            single_branch,
            branch: branch.clone(),
            tag_opt: tag_opt.as_deref(),
            partial_clone_filter: partial_clone_filter.as_deref(),
            template: template.as_deref(),
            template_config,
            separate_git_dir: separate_git_dir.as_deref(),
            config_overrides: &config_overrides,
            submodule_active: &submodule_active,
            revision: revision.as_deref(),
            shared,
            reference_alternates: &reference_alternates,
            bundle_uri: bundle_uri.as_ref(),
            depth,
        });
    }
    if fetch_source_is_ssh(&repository)? {
        return clone_ssh_repository(CloneHttpOptions {
            repository: &repository,
            destination: &destination,
            destination_display: &destination_display,
            origin: &origin,
            quiet,
            bare,
            checkout,
            sparse,
            single_branch,
            branch: branch.clone(),
            tag_opt: tag_opt.as_deref(),
            partial_clone_filter: partial_clone_filter.as_deref(),
            template: template.as_deref(),
            template_config,
            separate_git_dir: separate_git_dir.as_deref(),
            config_overrides: &config_overrides,
            submodule_active: &submodule_active,
            revision: revision.as_deref(),
            shared,
            reference_alternates: &reference_alternates,
            bundle_uri: bundle_uri.as_ref(),
            depth,
        });
    }

    let remote_git_dir = ls_remote_git_dir(&repository)?;
    let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
    let format = repository_object_format(&remote_common_git_dir)?;
    let remote_head_branch = remote_head_branch(&remote_common_git_dir, format)?;
    let alternates = clone_alternates(&remote_git_dir, shared, &reference_alternates)?;
    let revision_oid = revision
        .as_deref()
        .map(|rev| resolve_revision(&remote_common_git_dir, format, rev))
        .transpose()?;
    let branch_explicit = branch.is_some();
    let checkout_branch = branch.unwrap_or_else(|| remote_head_branch.clone());
    // git only treats a clone as "local" (hardlink/copy mechanism, shallow
    // options warned-and-ignored) when the source resolves as a plain path and
    // `--no-local` was not given: `is_local = option_local != 0 && path &&
    // !is_bundle` in builtin/clone.c, and a `file://` URL never resolves as a
    // path (`get_repo_path` stats the raw string). A non-local path clone goes
    // through the transport, which honors `--depth`. The bare and `--revision`
    // paths below bypass the transport fetch and cannot deepen, so the
    // warn-and-ignore is kept for them.
    let local_mechanism = local != Some(false)
        && !parse_remote_url(&repository)
            .map(|url| url.transport == RemoteTransport::File)
            .unwrap_or(false);
    if !quiet {
        if bare {
            eprintln!(
                "Cloning into bare repository '{}'...",
                destination_display.display()
            );
        } else {
            eprintln!("Cloning into '{}'...", destination_display.display());
        }
    }
    // The shallow-option and filter warnings follow the "Cloning into" line,
    // matching upstream's order (builtin/clone.c prints the banner before the
    // is_local checks).
    let depth = if depth.is_some() && (local_mechanism || bare || revision.is_some()) {
        eprintln!("warning: --depth is ignored in local clones; use file:// instead.");
        None
    } else {
        depth
    };
    if shallow_since_ignored {
        eprintln!("warning: --shallow-since is ignored in local clones; use file:// instead.");
    }
    if shallow_exclude_ignored {
        eprintln!("warning: --shallow-exclude is ignored in local clones; use file:// instead.");
    }
    if partial_clone_filter.is_some() {
        let file_transport = parse_remote_url(&repository)
            .map(|url| url.transport == RemoteTransport::File)
            .unwrap_or(false);
        if file_transport {
            eprintln!("warning: filtering not recognized by server, ignoring");
        } else {
            eprintln!("warning: --filter is ignored in local clones; use file:// instead.");
        }
    }
    // git prints the trailing "done." only for clones served by `clone_local`
    // in upstream `builtin/clone.c`, i.e. when the source is a plain local
    // path. A `file://` source goes through the transport machinery upstream
    // (even though sley serves it from this same local code path), so it ends
    // without "done.". This is a strictly narrower condition than
    // `local_mechanism` (which also covers `--local` over a `file://`-less path
    // for the depth warn-and-ignore), so the two are kept distinct — and
    // `--no-local` routes a plain local path through the transport machinery,
    // so "done." additionally requires the local mechanism to have engaged.
    let local_source = local_mechanism
        && parse_remote_url(&repository)
            .map(|url| url.transport == RemoteTransport::Local)
            .unwrap_or(false);
    if bare {
        clone_bare_or_mirror_local_repository(
            &destination,
            CloneLocalOptions {
                format,
                origin: &origin,
                repository: &repository,
                tag_opt: tag_opt.as_deref(),
                partial_clone_filter: partial_clone_filter.as_deref(),
                head_branch: &checkout_branch,
                branch_explicit,
                revision_oid: revision_oid.as_ref(),
                mirror,
                single_branch,
                template: template.as_deref(),
                template_config,
                bundle_uri: bundle_uri.as_ref(),
                alternates: &alternates,
                dissociate,
                config_overrides: &config_overrides,
                submodule_active: &submodule_active,
            },
        )?;
        if !quiet && local_source {
            eprintln!("done.");
        }
        return Ok(());
    }
    // Apply the post-init repository config common to both the revision and the
    // branch-tracking local clone: template, alternates, the origin remote (with
    // the given fetch refspec — `None` for `--revision`), `-c` overrides,
    // `submodule.active`, and any `--bundle-uri`. Returns the resulting config.
    let configure_local_clone =
        |git_dir: &Path, fetch_refspec: Option<String>| -> Result<GitConfig> {
            apply_clone_template(git_dir, template.as_deref(), template_config)?;
            apply_clone_alternates(git_dir, &alternates, dissociate)?;
            configure_clone_remote(
                git_dir,
                &origin,
                &repository,
                fetch_refspec,
                false,
                tag_opt.as_deref(),
                partial_clone_filter.as_deref(),
            )?;
            apply_clone_config_overrides(git_dir, &config_overrides)?;
            apply_clone_submodule_active(git_dir, &submodule_active)?;
            if let Some(bundle_uri) = bundle_uri.as_ref() {
                apply_clone_bundle_uri(git_dir, format, bundle_uri)?;
            }
            read_repo_config(git_dir)
        };

    if let Some(revision_oid) = revision_oid.as_ref() {
        // `--revision` copies the object closure directly and checks out detached;
        // it never fetches or creates a branch, so it keeps its own init here.
        let layout = RepositoryLayout::init_at_with_initial_branch(
            &destination,
            format,
            false,
            "__git_rs_clone_unborn__",
        )?;
        let git_dir = layout.git_dir;
        configure_local_clone(&git_dir, None)?;
        copy_local_revision_objects(&remote_common_git_dir, &git_dir, format, revision_oid)?;
        if checkout {
            let config = read_repo_config(&git_dir)?;
            sley_worktree::checkout_detached_filtered(
                &destination,
                &git_dir,
                format,
                revision_oid,
                commit_identity_from_env("COMMITTER")?,
                format!("clone: from {repository}").into_bytes(),
                &config,
            )?;
            print_clone_detached_head_advice(revision_oid);
        } else {
            sley_worktree::checkout_detached(
                &destination,
                &git_dir,
                format,
                revision_oid,
                commit_identity_from_env("COMMITTER")?,
                format!("clone: from {repository}").into_bytes(),
            )?;
            remove_clone_worktree_files(&destination, &git_dir, format)?;
        }
        if let Some(separate_git_dir) = separate_git_dir.as_deref() {
            apply_clone_separate_git_dir(&destination, &git_dir, separate_git_dir)?;
        }
        if !quiet && local_source {
            eprintln!("done.");
        }
        return Ok(());
    }

    let remote_source = sley_remote::CloneSource::Local {
        git_dir: remote_git_dir,
        common_git_dir: remote_common_git_dir,
    };
    let clone_options = sley_remote::CloneOptions {
        origin: &origin,
        checkout_branch: &checkout_branch,
        remote_head_branch: &remote_head_branch,
        single_branch,
        // A non-local path clone (`--no-local` / `file://`) honors `--depth`
        // through the in-process transport; a plain local clone had its depth
        // warned-and-ignored above, leaving `None` (a full clone).
        depth,
        committer: commit_identity_from_env("COMMITTER")?,
    };
    let mut credentials = sley_remote::NoCredentials;
    let mut progress = StdoutProgress;
    let outcome = sley_remote::clone(
        sley_remote::CloneRequest {
            destination: &destination,
            format,
            source: &remote_source,
            options: &clone_options,
        },
        sley_remote::CloneServices {
            configure: &mut |git_dir| {
                let fetch_refspec = if single_branch {
                    Some(format!(
                        "+refs/heads/{checkout_branch}:refs/remotes/{origin}/{checkout_branch}"
                    ))
                } else {
                    Some(format!("+refs/heads/*:refs/remotes/{origin}/*"))
                };
                configure_local_clone(git_dir, fetch_refspec)
            },
            configure_branch: &mut |git_dir, branch| {
                configure_clone_branch(git_dir, branch, &origin)?;
                read_repo_config(git_dir)
            },
            credentials: &mut credentials,
            progress: &mut progress,
        },
    )?;
    let git_dir = outcome.git_dir;
    if !checkout {
        remove_clone_worktree_files(&destination, &git_dir, format)?;
    } else if sparse {
        apply_clone_sparse_checkout(&destination, &git_dir, format)?;
    }
    if let Some(separate_git_dir) = separate_git_dir.as_deref() {
        apply_clone_separate_git_dir(&destination, &git_dir, separate_git_dir)?;
    }
    if !quiet && local_source {
        eprintln!("done.");
    }
    Ok(())
}

struct CloneHttpOptions<'a> {
    repository: &'a str,
    destination: &'a Path,
    /// The destination as given on the command line (or derived from the
    /// source), for user-facing messages — `dir` in upstream `builtin/clone.c`.
    destination_display: &'a Path,
    origin: &'a str,
    quiet: bool,
    bare: bool,
    checkout: bool,
    sparse: bool,
    single_branch: bool,
    branch: Option<String>,
    tag_opt: Option<&'a str>,
    partial_clone_filter: Option<&'a str>,
    template: Option<&'a Path>,
    template_config: bool,
    separate_git_dir: Option<&'a Path>,
    config_overrides: &'a [GlobalConfigOverride],
    submodule_active: &'a [String],
    revision: Option<&'a str>,
    shared: bool,
    reference_alternates: &'a [CloneReferenceAlternate],
    bundle_uri: Option<&'a CloneBundleUri>,
    depth: Option<u32>,
}

/// Derive the remote default branch name from the upload-pack advertisement:
/// prefer the advertised `HEAD` symref, otherwise match the `HEAD` object id to a
/// branch tip.
fn http_remote_head_branch(
    features: &UploadPackFeatures,
    advertisements: &[RefAdvertisement],
) -> Result<String> {
    for symref in &features.symrefs {
        if let Some((name, target)) = symref.split_once(':')
            && name == "HEAD"
            && let Some(branch) = target.strip_prefix("refs/heads/")
        {
            return Ok(branch.to_string());
        }
    }
    if let Some(head) = advertisements
        .iter()
        .find(|advertisement| advertisement.name == "HEAD")
    {
        for advertisement in advertisements {
            if advertisement.oid == head.oid
                && let Some(branch) = advertisement.name.strip_prefix("refs/heads/")
            {
                return Ok(branch.to_string());
            }
        }
    }
    Err(GitError::Unsupported(
        "could not determine the remote default branch".into(),
    ))
}

/// Clone a repository over smart HTTP(S). Covers the common non-bare case;
/// bare/mirror, `--revision`, `--shared`/`--reference`, `--bundle-uri`, and
/// SHA-256 remotes are not supported over HTTP yet.
fn clone_http_repository(options: CloneHttpOptions<'_>) -> Result<()> {
    if options.bare {
        return Err(GitError::Unsupported(
            "cloning bare/mirror repositories over HTTP is not supported yet".into(),
        ));
    }
    if options.revision.is_some() {
        return Err(GitError::Unsupported(
            "clone --revision over HTTP is not supported yet".into(),
        ));
    }
    if options.shared || !options.reference_alternates.is_empty() {
        return Err(GitError::Unsupported(
            "clone --shared/--reference over HTTP is not supported yet".into(),
        ));
    }
    if options.bundle_uri.is_some() {
        return Err(GitError::Unsupported(
            "clone --bundle-uri over HTTP is not supported yet".into(),
        ));
    }
    if options.partial_clone_filter.is_some() {
        eprintln!("warning: --filter is not supported over HTTP yet, ignoring");
    }

    let remote = parse_remote_url(&ls_remote_resolved_url(options.repository)?)?;
    let client = sley_remote::new_http_client();
    let mut credentials = sley_remote::NoCredentials;
    let (advertisements, features) = sley_remote::http_upload_pack_advertisements(
        &client,
        &remote,
        ObjectFormat::Sha1,
        &mut credentials,
    )?;
    let format = features.object_format.unwrap_or(ObjectFormat::Sha1);
    if format != ObjectFormat::Sha1 {
        return Err(GitError::Unsupported(format!(
            "cloning {} repositories over HTTP is not supported yet",
            format.name()
        )));
    }
    let remote_head_branch = http_remote_head_branch(&features, &advertisements)?;
    let branch_explicit = options.branch.is_some();
    let checkout_branch = options
        .branch
        .clone()
        .unwrap_or_else(|| remote_head_branch.clone());

    if !options.quiet {
        eprintln!("Cloning into '{}'...", options.destination_display.display());
    }

    let single_branch = options.single_branch;
    let origin = options.origin;
    let repository = options.repository;
    let template = options.template;
    let template_config = options.template_config;
    let tag_opt = options.tag_opt;
    let config_overrides = options.config_overrides;
    let submodule_active = options.submodule_active;
    let remote_source = sley_remote::CloneSource::Http(remote);
    let clone_options = sley_remote::CloneOptions {
        origin,
        checkout_branch: &checkout_branch,
        remote_head_branch: &remote_head_branch,
        single_branch,
        depth: options.depth,
        committer: commit_identity_from_env("COMMITTER")?,
    };
    let mut progress = StdoutProgress;
    let outcome = sley_remote::clone(
        sley_remote::CloneRequest {
            destination: options.destination,
            format,
            source: &remote_source,
            options: &clone_options,
        },
        sley_remote::CloneServices {
            configure: &mut |git_dir| {
                apply_clone_template(git_dir, template, template_config)?;
                let fetch_refspec = if single_branch {
                    Some(format!(
                        "+refs/heads/{checkout_branch}:refs/remotes/{origin}/{checkout_branch}"
                    ))
                } else {
                    Some(format!("+refs/heads/*:refs/remotes/{origin}/*"))
                };
                configure_clone_remote(
                    git_dir,
                    origin,
                    repository,
                    fetch_refspec,
                    false,
                    tag_opt,
                    None,
                )?;
                apply_clone_config_overrides(git_dir, config_overrides)?;
                apply_clone_submodule_active(git_dir, submodule_active)?;
                read_repo_config(git_dir)
            },
            configure_branch: &mut |git_dir, branch| {
                configure_clone_branch(git_dir, branch, origin)?;
                read_repo_config(git_dir)
            },
            credentials: &mut credentials,
            progress: &mut progress,
        },
    );
    let outcome = map_clone_missing_branch(outcome, branch_explicit, &checkout_branch, origin)?;
    let git_dir = outcome.git_dir;

    if !options.checkout {
        remove_clone_worktree_files(options.destination, &git_dir, format)?;
    } else if options.sparse {
        apply_clone_sparse_checkout(options.destination, &git_dir, format)?;
    }
    if let Some(separate_git_dir) = options.separate_git_dir {
        apply_clone_separate_git_dir(options.destination, &git_dir, separate_git_dir)?;
    }
    if !options.quiet {
        eprintln!("done.");
    }
    Ok(())
}

/// Clone a repository over SSH upload-pack. Covers the common non-bare case;
/// bare/mirror, `--revision`, `--shared`/`--reference`, and `--bundle-uri` are
/// not supported over SSH yet.
fn clone_ssh_repository(options: CloneHttpOptions<'_>) -> Result<()> {
    if options.bare {
        return Err(GitError::Unsupported(
            "cloning bare/mirror repositories over SSH is not supported yet".into(),
        ));
    }
    if options.revision.is_some() {
        return Err(GitError::Unsupported(
            "clone --revision over SSH is not supported yet".into(),
        ));
    }
    if options.shared || !options.reference_alternates.is_empty() {
        return Err(GitError::Unsupported(
            "clone --shared/--reference over SSH is not supported yet".into(),
        ));
    }
    if options.bundle_uri.is_some() {
        return Err(GitError::Unsupported(
            "clone --bundle-uri over SSH is not supported yet".into(),
        ));
    }
    if options.partial_clone_filter.is_some() {
        eprintln!("warning: --filter is not supported over SSH yet, ignoring");
    }

    let remote = parse_remote_url(&ls_remote_resolved_url(options.repository)?)?;
    let (advertisements, features) =
        sley_remote::ssh_upload_pack_advertisements(&remote, ObjectFormat::Sha1)?;
    let format = features.object_format.unwrap_or(ObjectFormat::Sha1);
    let remote_head_branch = http_remote_head_branch(&features, &advertisements)?;
    let branch_explicit = options.branch.is_some();
    let checkout_branch = options
        .branch
        .clone()
        .unwrap_or_else(|| remote_head_branch.clone());

    if !options.quiet {
        eprintln!("Cloning into '{}'...", options.destination_display.display());
    }

    let single_branch = options.single_branch;
    let origin = options.origin;
    let repository = options.repository;
    let template = options.template;
    let template_config = options.template_config;
    let tag_opt = options.tag_opt;
    let config_overrides = options.config_overrides;
    let submodule_active = options.submodule_active;
    let remote_source = sley_remote::CloneSource::Ssh(remote);
    let clone_options = sley_remote::CloneOptions {
        origin,
        checkout_branch: &checkout_branch,
        remote_head_branch: &remote_head_branch,
        single_branch,
        depth: options.depth,
        committer: commit_identity_from_env("COMMITTER")?,
    };
    let mut credentials = sley_remote::NoCredentials;
    let mut progress = StdoutProgress;
    let outcome = sley_remote::clone(
        sley_remote::CloneRequest {
            destination: options.destination,
            format,
            source: &remote_source,
            options: &clone_options,
        },
        sley_remote::CloneServices {
            configure: &mut |git_dir| {
                apply_clone_template(git_dir, template, template_config)?;
                let fetch_refspec = if single_branch {
                    Some(format!(
                        "+refs/heads/{checkout_branch}:refs/remotes/{origin}/{checkout_branch}"
                    ))
                } else {
                    Some(format!("+refs/heads/*:refs/remotes/{origin}/*"))
                };
                configure_clone_remote(
                    git_dir,
                    origin,
                    repository,
                    fetch_refspec,
                    false,
                    tag_opt,
                    None,
                )?;
                apply_clone_config_overrides(git_dir, config_overrides)?;
                apply_clone_submodule_active(git_dir, submodule_active)?;
                read_repo_config(git_dir)
            },
            configure_branch: &mut |git_dir, branch| {
                configure_clone_branch(git_dir, branch, origin)?;
                read_repo_config(git_dir)
            },
            credentials: &mut credentials,
            progress: &mut progress,
        },
    );
    let outcome = map_clone_missing_branch(outcome, branch_explicit, &checkout_branch, origin)?;
    let git_dir = outcome.git_dir;

    if !options.checkout {
        remove_clone_worktree_files(options.destination, &git_dir, format)?;
    } else if options.sparse {
        apply_clone_sparse_checkout(options.destination, &git_dir, format)?;
    }
    if let Some(separate_git_dir) = options.separate_git_dir {
        apply_clone_separate_git_dir(options.destination, &git_dir, separate_git_dir)?;
    }
    if !options.quiet {
        eprintln!("done.");
    }
    Ok(())
}

/// Map a [`sley_remote::clone`] result that failed because the requested branch
/// was absent from the remote into the CLI's explicit-`--branch` message, leaving
/// every other result untouched. `git clone -b <missing>` prints a dedicated
/// "Remote branch … not found" line and exits 128; without an explicit branch the
/// generic not-found error propagates.
fn map_clone_missing_branch(
    outcome: Result<sley_remote::CloneOutcome>,
    branch_explicit: bool,
    checkout_branch: &str,
    origin: &str,
) -> Result<sley_remote::CloneOutcome> {
    match outcome {
        Err(GitError::NotFound(kind))
            if branch_explicit
                && kind.to_string()
                    == format!("remote ref refs/remotes/{origin}/{checkout_branch}") =>
        {
            eprintln!("fatal: Remote branch {checkout_branch} not found in upstream {origin}");
            Err(GitError::Exit(128))
        }
        other => other,
    }
}

fn clone_jobs_error() -> &'static str {
    "error: option `jobs' expects an integer value with an optional k/m/g suffix"
}

/// Parse a `--depth` value the way `git clone`/`git fetch` do: an optional `+`
/// sign then ASCII digits, rejecting non-positive depths with git's message. The
/// numeric value is clamped to `u32::MAX` (git stores depth as a C `int`; the
/// protocol's `deepen` is unsigned, and any value this large already deepens past
/// every real history).
fn parse_clone_depth(value: &str) -> Result<u32> {
    let digits = value.strip_prefix('+').unwrap_or(value);
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(GitError::Command(format!(
            "fatal: depth {value} is not a positive number"
        )));
    }
    if !digits.chars().any(|ch| ch != '0') {
        return Err(GitError::Command(format!(
            "fatal: depth {value} is not a positive number"
        )));
    }
    Ok(digits.parse::<u32>().unwrap_or(u32::MAX))
}

fn validate_clone_jobs(value: &str) -> Result<()> {
    let mut chars = value.chars();
    let first = chars
        .next()
        .ok_or_else(|| GitError::Command(clone_jobs_error().into()))?;
    let mut saw_digit = false;
    let mut suffix_seen = false;
    let digits = if first == '+' || first == '-' {
        chars.as_str()
    } else {
        value
    };
    for ch in digits.chars() {
        if suffix_seen {
            return Err(GitError::Command(clone_jobs_error().into()));
        }
        if ch.is_ascii_digit() {
            saw_digit = true;
        } else if saw_digit && matches!(ch, 'k' | 'K' | 'm' | 'M' | 'g' | 'G') {
            suffix_seen = true;
        } else {
            return Err(GitError::Command(clone_jobs_error().into()));
        }
    }
    if saw_digit {
        Ok(())
    } else {
        Err(GitError::Command(clone_jobs_error().into()))
    }
}

fn validate_clone_filter(value: &str) -> Result<()> {
    if value == "blob:none" {
        return Ok(());
    }
    if let Some(depth) = value.strip_prefix("tree:") {
        parse_rev_list_tree_depth(depth)?;
        return Ok(());
    }
    if let Some(limit) = value.strip_prefix("blob:limit=") {
        parse_rev_list_blob_limit(limit)?;
        return Ok(());
    }
    if let Some(object_type) = value.strip_prefix("object:type=") {
        parse_rev_list_object_type_filter(object_type)?;
        return Ok(());
    }
    eprintln!("fatal: invalid filter-spec '{value}'");
    Err(GitError::Exit(128))
}

#[derive(Debug, Clone)]
struct CloneReferenceAlternate {
    path: String,
    if_able: bool,
}

struct CloneBundleUri {
    uri: String,
    path: PathBuf,
}

impl CloneBundleUri {
    fn new(cwd: &Path, uri: &str) -> Self {
        let path = uri
            .strip_prefix("file://")
            .map(PathBuf::from)
            .unwrap_or_else(|| resolve_cli_path(cwd, uri));
        Self {
            uri: uri.to_string(),
            path,
        }
    }
}

struct CloneLocalOptions<'a> {
    format: ObjectFormat,
    origin: &'a str,
    repository: &'a str,
    tag_opt: Option<&'a str>,
    partial_clone_filter: Option<&'a str>,
    head_branch: &'a str,
    branch_explicit: bool,
    revision_oid: Option<&'a ObjectId>,
    mirror: bool,
    single_branch: bool,
    template: Option<&'a Path>,
    template_config: bool,
    bundle_uri: Option<&'a CloneBundleUri>,
    alternates: &'a [PathBuf],
    dissociate: bool,
    config_overrides: &'a [GlobalConfigOverride],
    submodule_active: &'a [String],
}

fn clone_bare_or_mirror_local_repository(
    destination: &Path,
    options: CloneLocalOptions<'_>,
) -> Result<()> {
    let layout = RepositoryLayout::init_at_with_initial_branch(
        destination,
        options.format,
        true,
        options.head_branch,
    )?;
    let git_dir = layout.git_dir;
    apply_clone_template(&git_dir, options.template, options.template_config)?;
    apply_clone_alternates(&git_dir, options.alternates, options.dissociate)?;
    let remote_refspec = options.mirror.then(|| "+refs/*:refs/*".to_string());
    let remote_tag_opt = if options.mirror {
        Some("--no-tags")
    } else {
        options.tag_opt
    };
    configure_clone_remote(
        &git_dir,
        options.origin,
        options.repository,
        remote_refspec,
        options.mirror,
        remote_tag_opt,
        options.partial_clone_filter,
    )?;
    apply_clone_config_overrides(&git_dir, options.config_overrides)?;
    apply_clone_submodule_active(&git_dir, options.submodule_active)?;
    if let Some(bundle_uri) = options.bundle_uri {
        apply_clone_bundle_uri(&git_dir, options.format, bundle_uri)?;
    }

    if let Some(revision_oid) = options.revision_oid {
        copy_local_revision_objects(
            &common_git_dir_for_git_dir(&ls_remote_git_dir(options.repository)?)?,
            &git_dir,
            options.format,
            revision_oid,
        )?;
        fs::write(git_dir.join("HEAD"), format!("{revision_oid}\n"))?;
        return Ok(());
    }

    let previous_cwd = env::current_dir()?;
    env::set_current_dir(destination)?;
    let refspecs = if options.mirror && options.single_branch {
        vec![format!(
            "+refs/heads/{}:refs/heads/{}",
            options.head_branch, options.head_branch
        )]
    } else if options.mirror {
        vec!["+refs/*:refs/*".to_string()]
    } else if options.single_branch {
        vec![format!(
            "+refs/heads/{}:refs/heads/{}",
            options.head_branch, options.head_branch
        )]
    } else {
        vec!["+refs/heads/*:refs/heads/*".to_string()]
    };
    let fetch_result = fetch_local_repository(
        &git_dir,
        options.format,
        options.origin,
        &refspecs,
        FetchOptions {
            quiet: true,
            auto_follow_tags: !options.mirror
                && (options.tag_opt != Some("--no-tags") || options.branch_explicit),
            fetch_all_tags: options.tag_opt == Some("--tags"),
            prune: false,
            dry_run: false,
            append: false,
            write_fetch_head: false,
            tag_option_explicit: options.tag_opt.is_some(),
            prune_option_explicit: false,
            depth: None,
            merge_src: None,
        },
    );
    env::set_current_dir(previous_cwd)?;
    fetch_result
}

/// Parse a `git clone --config <key>=<value>` (a.k.a. clone's own `-c`) entry
/// into a [`GlobalConfigOverride`] to persist into the cloned repository's config.
/// A missing `=` makes the value boolean-true; an empty key is rejected. This is
/// distinct from the global `git -c` injection (which never persists).
fn parse_clone_config_override(value: &str) -> Result<GlobalConfigOverride> {
    let Some((key, val)) = value.split_once('=') else {
        return Ok(GlobalConfigOverride {
            key: value.to_string(),
            value: "true".to_string(),
        });
    };
    if key.is_empty() {
        eprintln!("error: key does not contain a section: {value}");
        return Err(GitError::Exit(128));
    }
    Ok(GlobalConfigOverride {
        key: key.to_string(),
        value: val.to_string(),
    })
}

fn apply_clone_config_overrides(git_dir: &Path, overrides: &[GlobalConfigOverride]) -> Result<()> {
    if overrides.is_empty() {
        return Ok(());
    }
    let mut config = read_repo_config(git_dir)?;
    for override_entry in overrides {
        let key = parse_config_key(&override_entry.key)?;
        config_set_value(&mut config, &key, &override_entry.value, false);
    }
    write_repo_config(git_dir, &config)
}

fn apply_clone_template(git_dir: &Path, template: Option<&Path>, copy_config: bool) -> Result<()> {
    let Some(template) = template else {
        return Ok(());
    };
    if !template.is_dir() {
        eprintln!("warning: templates not found in {}", template.display());
        return Ok(());
    }
    copy_clone_template_entries(template, git_dir)?;
    let template_config_path = template.join("config");
    if copy_config && template_config_path.is_file() {
        let mut template_config = GitConfig::read(template_config_path)?;
        let current_config = read_repo_config(git_dir)?;
        template_config.sections.extend(current_config.sections);
        write_repo_config(git_dir, &template_config)?;
    }
    Ok(())
}

fn copy_clone_template_entries(source: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let source_path = entry.path();
        let destination_path = destination.join(&name);
        if source_path.is_dir() {
            if !destination_path.exists() {
                fs::create_dir_all(&destination_path)?;
            }
            copy_clone_template_entries(&source_path, &destination_path)?;
        } else if name != "config" && !destination_path.exists() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(source_path, destination_path)?;
        }
    }
    Ok(())
}

fn clone_alternates(
    remote_git_dir: &Path,
    shared: bool,
    references: &[CloneReferenceAlternate],
) -> Result<Vec<PathBuf>> {
    let mut alternates = Vec::new();
    if shared {
        alternates.push(remote_git_dir.join("objects"));
    }
    for reference in references {
        match ls_remote_git_dir(&reference.path)
            .and_then(|git_dir| common_git_dir_for_git_dir(&git_dir))
        {
            Ok(reference_git_dir) => alternates.push(reference_git_dir.join("objects")),
            Err(_) if reference.if_able => eprintln!(
                "info: Could not add alternate for '{}': reference repository '{}' is not a local repository.",
                reference.path, reference.path
            ),
            Err(err) => return Err(err),
        }
    }
    Ok(alternates)
}

fn apply_clone_alternates(git_dir: &Path, alternates: &[PathBuf], dissociate: bool) -> Result<()> {
    if alternates.is_empty() || dissociate {
        return Ok(());
    }
    let alternates_path = git_dir.join("objects/info/alternates");
    let mut contents = String::new();
    for alternate in alternates {
        contents.push_str(&alternate.to_string_lossy());
        contents.push('\n');
    }
    fs::write(alternates_path, contents)?;
    Ok(())
}

fn copy_local_revision_objects(
    remote_git_dir: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    revision_oid: &ObjectId,
) -> Result<()> {
    let remote_db = FileObjectDatabase::from_git_dir(remote_git_dir, format);
    let local_db = FileObjectDatabase::from_git_dir(git_dir, format);
    install_reachable_pack(
        &remote_db,
        &local_db,
        format,
        std::iter::once(*revision_oid),
    )
    .map(|_| ())
}

fn apply_clone_bundle_uri(
    git_dir: &Path,
    format: ObjectFormat,
    bundle_uri: &CloneBundleUri,
) -> Result<()> {
    let bundle = match fs::read(&bundle_uri.path).and_then(|bytes| {
        Bundle::parse(&bytes, format).map_err(|err| io::Error::other(err.to_string()))
    }) {
        Ok(bundle) => bundle,
        Err(_) => {
            warn_clone_bundle_uri_failed(&bundle_uri.uri);
            return Ok(());
        }
    };
    let prerequisite_reader = FileObjectDatabase::from_git_dir(git_dir, format);
    let database = FileObjectDatabase::from_git_dir(git_dir, format);
    let result = match install_bundle_pack(&bundle, &prerequisite_reader, &database) {
        Ok(result) => result,
        Err(_) => {
            warn_clone_bundle_uri_failed(&bundle_uri.uri);
            return Ok(());
        }
    };
    let updates = result
        .references
        .iter()
        .filter_map(|reference| {
            clone_bundle_uri_ref_name(&reference.name).map(|name| BundleRefUpdate {
                name,
                oid: reference.oid,
            })
        })
        .collect::<Vec<_>>();
    FileRefStore::new(git_dir, format)
        .apply_bundle_ref_updates(&updates, None)
        .map(|_| ())
}

fn clone_bundle_uri_ref_name(name: &str) -> Option<String> {
    name.strip_prefix("refs/")
        .map(|name| format!("refs/bundles/{name}"))
}

fn warn_clone_bundle_uri_failed(uri: &str) {
    eprintln!("warning: failed to download bundle from URI '{uri}'");
    eprintln!("warning: failed to fetch objects from bundle URI '{uri}'");
}

fn apply_clone_submodule_active(git_dir: &Path, active: &[String]) -> Result<()> {
    if active.is_empty() {
        return Ok(());
    }
    let mut config = read_repo_config(git_dir)?;
    let key = parse_config_key("submodule.active")?;
    for value in active {
        config_set_value(&mut config, &key, value, true);
    }
    write_repo_config(git_dir, &config)
}

fn apply_clone_sparse_checkout(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let index = sley_worktree::read_repository_index(git_dir, format)?.unwrap_or(Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    });
    let sparse_paths = index
        .entries
        .iter()
        .filter(|entry| entry.path.contains(&b'/'))
        .map(|entry| PathBuf::from(String::from_utf8_lossy(&entry.path).as_ref()))
        .collect::<Vec<_>>();
    if !sparse_paths.is_empty() {
        sley_worktree::set_index_skip_worktree_paths(
            worktree_root,
            git_dir,
            format,
            &sparse_paths,
            true,
        )?;
        for path in sparse_paths {
            let path =
                checkout_index_worktree_path(worktree_root, path.to_string_lossy().as_bytes())?;
            if path.exists() {
                fs::remove_file(&path)?;
                prune_empty_clone_dirs(worktree_root, path.parent())?;
            }
        }
    }
    fs::create_dir_all(git_dir.join("info"))?;
    fs::write(git_dir.join("info/sparse-checkout"), b"/*\n!/*/\n")?;

    let mut config = read_repo_config(git_dir)?;
    let key = parse_config_key("extensions.worktreeConfig")?;
    config_set_value(&mut config, &key, "true", false);
    write_repo_config(git_dir, &config)?;

    let worktree_config = GitConfig {
        sections: vec![ConfigSection::new(
            "core",
            None,
            vec![
                ConfigEntry::new("sparsecheckout", Some("true".into())),
                ConfigEntry::new("sparsecheckoutcone", Some("true".into())),
            ],
        )],
        ..Default::default()
    };
    fs::write(
        git_dir.join("config.worktree"),
        worktree_config.to_canonical_bytes(),
    )?;
    Ok(())
}

fn print_clone_detached_head_advice(oid: &ObjectId) {
    eprintln!(
        "Note: switching to '{oid}'.

You are in 'detached HEAD' state. You can look around, make experimental
changes and commit them, and you can discard any commits you make in this
state without impacting any branches by switching back to a branch.

If you want to create a new branch to retain commits you create, you may
do so (now or later) by using -c with the switch command. Example:

  git switch -c <new-branch-name>

Or undo this operation with:

  git switch -

Turn off this advice by setting config variable advice.detachedHead to false
"
    );
}

fn apply_clone_separate_git_dir(
    worktree_root: &Path,
    git_dir: &Path,
    separate_git_dir: &Path,
) -> Result<()> {
    if let Some(parent) = separate_git_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    if separate_git_dir.exists() {
        return Err(GitError::Command(format!(
            "separate git dir {} already exists",
            separate_git_dir.display()
        )));
    }
    fs::rename(git_dir, separate_git_dir)?;
    let canonical = fs::canonicalize(separate_git_dir)?;
    fs::write(
        worktree_root.join(".git"),
        format!("gitdir: {}\n", canonical.display()),
    )?;
    Ok(())
}

fn remove_clone_worktree_files(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let index = sley_worktree::read_repository_index(git_dir, format)?.unwrap_or(Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    });
    for entry in &index.entries {
        let path = checkout_index_worktree_path(worktree_root, &entry.path)?;
        if path.exists() {
            fs::remove_file(&path)?;
            prune_empty_clone_dirs(worktree_root, path.parent())?;
        }
    }
    fs::write(
        sley_worktree::repository_index_path(git_dir),
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(())
}

fn checkout_index_worktree_path(root: &Path, path: &[u8]) -> Result<PathBuf> {
    let text = std::str::from_utf8(path).map_err(|err| GitError::InvalidPath(err.to_string()))?;
    let relative = PathBuf::from(text);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(GitError::InvalidPath(format!(
            "invalid worktree path {text}"
        )));
    }
    Ok(root.join(relative))
}

fn prune_empty_clone_dirs(root: &Path, mut dir: Option<&Path>) -> Result<()> {
    while let Some(path) = dir {
        if path == root {
            break;
        }
        match fs::remove_dir(path) {
            Ok(()) => dir = path.parent(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => dir = path.parent(),
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn default_clone_directory(repository: &str, bare: bool) -> PathBuf {
    let path = parse_remote_url(repository)
        .ok()
        .map(|url| PathBuf::from(url.path))
        .unwrap_or_else(|| PathBuf::from(repository));
    let leaf = path.file_name().and_then(|name| {
        if name == ".git" {
            path.parent().and_then(Path::file_name)
        } else {
            Some(name)
        }
    });
    let name = leaf
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repository".to_string());
    let name = name.strip_suffix(".git").unwrap_or(&name);
    if bare {
        PathBuf::from(format!("{name}.git"))
    } else {
        PathBuf::from(name)
    }
}

fn remote_head_branch(remote_git_dir: &Path, format: ObjectFormat) -> Result<String> {
    let remote_store = FileRefStore::new(remote_git_dir, format);
    match remote_store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => target
            .strip_prefix("refs/heads/")
            .map(str::to_string)
            .ok_or_else(|| GitError::reference_not_found("remote HEAD branch")),
        Some(RefTarget::Direct(_)) | None => {
            Err(GitError::reference_not_found("remote HEAD branch"))
        }
    }
}

fn configure_clone_remote(
    git_dir: &Path,
    name: &str,
    url: &str,
    fetch_refspec: Option<String>,
    mirror: bool,
    tag_opt: Option<&str>,
    partial_clone_filter: Option<&str>,
) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    let mut entries = vec![ConfigEntry::new("url", Some(url.to_string()))];
    if let Some(fetch_refspec) = fetch_refspec {
        entries.push(ConfigEntry::new("fetch", Some(fetch_refspec)));
    }
    if mirror {
        entries.push(ConfigEntry::new("mirror", Some("true".into())));
    }
    if let Some(tag_opt) = tag_opt {
        entries.push(ConfigEntry::new("tagopt", Some(tag_opt.to_string())));
    }
    if let Some(filter) = partial_clone_filter {
        entries.push(ConfigEntry::new("promisor", Some("true".into())));
        entries.push(ConfigEntry::new(
            "partialclonefilter",
            Some(filter.to_string()),
        ));
    }
    config.sections.push(ConfigSection::new(
        "remote",
        Some(name.to_string()),
        entries,
    ));
    write_repo_config(git_dir, &config)
}

fn configure_clone_branch(git_dir: &Path, branch: &str, remote: &str) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    config.sections.push(ConfigSection::new(
        "branch",
        Some(branch.to_string()),
        vec![
            ConfigEntry::new("remote", Some(remote.to_string())),
            ConfigEntry::new("merge", Some(format!("refs/heads/{branch}"))),
        ],
    ));
    write_repo_config(git_dir, &config)
}
pub(crate) fn cmd_fetch(args: &[String]) -> Result<()> {
    let mut source = None::<String>;
    let mut refspecs = Vec::new();
    let mut options = FetchOptions {
        quiet: false,
        auto_follow_tags: true,
        fetch_all_tags: false,
        prune: false,
        dry_run: false,
        append: false,
        write_fetch_head: true,
        tag_option_explicit: false,
        prune_option_explicit: false,
        depth: None,
        merge_src: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-q" | "--quiet" if source.is_none() => options.quiet = true,
            "--no-quiet" if source.is_none() => options.quiet = false,
            "--write-fetch-head" => options.write_fetch_head = true,
            "--no-write-fetch-head" => options.write_fetch_head = false,
            "--append" | "-a" => options.append = true,
            "--no-append" => options.append = false,
            "-n" | "--dry-run" => options.dry_run = true,
            "--no-dry-run" => options.dry_run = false,
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
            "--tags" => {
                options.auto_follow_tags = true;
                options.fetch_all_tags = true;
                options.tag_option_explicit = true;
            }
            "--no-tags" => {
                options.auto_follow_tags = false;
                options.fetch_all_tags = false;
                options.tag_option_explicit = true;
            }
            _ if source.is_none() => source = Some(arg.clone()),
            _ => refspecs.push(arg.clone()),
        }
    }
    let source = source.unwrap_or_else(|| "origin".to_string());
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    if let Ok(input) = fs::read(&source)
        && let Ok(bundle) = Bundle::parse(&input, format)
    {
        // Bundle fetches have no shallow support, so a `--depth` is warned-and-
        // ignored here, matching the local-clone behavior.
        if options.depth.is_some() {
            eprintln!("warning: --depth is ignored in bundle fetches; use file:// instead.");
        }
        return fetch_bundle(&git_dir, format, &source, &refspecs, &bundle, options);
    }
    if fetch_source_is_http(&source)? {
        return fetch_http_repository(&git_dir, format, &source, &refspecs, options);
    }
    if fetch_source_is_ssh(&source)? {
        return fetch_ssh_repository(&git_dir, format, &source, &refspecs, options);
    }
    // Local (`file://`/path) fetches keep the historical warn-and-ignore for
    // `--depth` (only clone wires the local deepen so far), so the depth is
    // cleared here and must not leak into the deepen-capable fetch below.
    if options.depth.is_some() {
        eprintln!("warning: --depth is ignored in local fetches; use file:// instead.");
        options.depth = None;
    }
    fetch_local_repository(&git_dir, format, &source, &refspecs, options)
}

pub(crate) fn cmd_receive_pack(args: &[String]) -> Result<()> {
    let repository = match args {
        [repository] => repository,
        _ => {
            return Err(GitError::Command(
                "receive-pack currently supports: receive-pack <repository>".into(),
            ));
        }
    };
    let git_dir = common_git_dir_for_git_dir(&ls_remote_git_dir(repository)?)?;
    let format = repository_object_format(&git_dir)?;
    let features = sley_remote::receive_pack_features(format);
    let mut advertisements = sley_remote::local_fetch_advertisements(&git_dir, format)?;
    sley_remote::attach_receive_pack_capabilities(&mut advertisements, format, &features)?;

    {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        write_ref_advertisement_set(
            &mut stdout,
            &RefAdvertisementSet {
                protocol: ProtocolVersion::V0,
                refs: advertisements,
                shallow: Vec::new(),
            },
        )?;
        stdout.flush()?;
    }

    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let commands = read_receive_pack_request(format, &mut stdin)?;
    let push_options = sley_remote::receive_pack_request_uses_push_options(&commands)
        .then(|| read_receive_pack_push_options(&mut stdin))
        .transpose()?;
    let mut packfile = Vec::new();
    stdin.read_to_end(&mut packfile)?;
    let request = ReceivePackPushRequest {
        commands,
        push_options,
        packfile,
    };
    let report = sley_remote::receive_pack_into_local_repository(&git_dir, format, &request)?;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_receive_pack_report_status(&mut stdout, &report)?;
    stdout.flush()?;
    Ok(())
}

pub(crate) fn cmd_upload_pack(args: &[String]) -> Result<()> {
    let repository = match args {
        [repository] => repository,
        _ => {
            return Err(GitError::Command(
                "upload-pack currently supports: upload-pack <repository>".into(),
            ));
        }
    };
    let git_dir = ls_remote_git_dir(repository)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let features = sley_remote::upload_pack_features(&git_dir, format)?;
    let mut advertisements = sley_remote::local_fetch_advertisements(&git_dir, format)?;
    sley_remote::attach_upload_pack_capabilities(&mut advertisements, format, &features)?;

    {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        write_ref_advertisement_set(
            &mut stdout,
            &RefAdvertisementSet {
                protocol: ProtocolVersion::V0,
                refs: advertisements,
                shallow: Vec::new(),
            },
        )?;
        stdout.flush()?;
    }

    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let Some(request) = read_upload_pack_request(format, &mut stdin)? else {
        return Ok(());
    };
    let mut haves = HashSet::new();
    loop {
        let negotiation = read_upload_pack_negotiation_request(format, &mut stdin)?;
        haves.extend(negotiation.haves);
        if negotiation.done {
            break;
        }
    }

    let sideband = sley_remote::upload_pack_request_uses_sideband(&request);
    let response = sley_remote::upload_pack_from_local_repository(
        &git_dir, format, &features, request, haves,
    )?;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    if sideband {
        let response = sley_remote::upload_pack_sideband_response(response);
        write_upload_pack_packfile_response(&mut stdout, &response)?;
    } else {
        write_upload_pack_raw_packfile_response(&mut stdout, &response)?;
    }
    stdout.flush()?;
    Ok(())
}

pub(crate) fn cmd_push(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let mut quiet = false;
    let mut set_upstream = false;
    let mut delete = false;
    let mut force = false;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-u" | "--set-upstream" => set_upstream = true,
            "--no-set-upstream" => set_upstream = false,
            "-d" | "--delete" => delete = true,
            "--no-delete" => delete = false,
            "--repo" | "--receive-pack" | "--exec" => {
                iter.next().ok_or_else(|| {
                    GitError::Command(format!("push {} requires a value", arg.as_str()))
                })?;
            }
            value
                if value.starts_with("--repo=")
                    || value.starts_with("--receive-pack=")
                    || value.starts_with("--exec=") => {}
            "--porcelain" | "--progress" | "--no-progress" | "--thin" | "--no-thin" => {}
            "--" => {
                positional.extend(iter.map(|value| value.to_string()));
                break;
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported push option {value}"
                )));
            }
            value => positional.push(value.to_string()),
        }
    }

    let (remote, refspecs) = if delete {
        let Some((remote, names)) = positional.split_first() else {
            return Err(GitError::Command(
                "push --delete requires at least one ref".into(),
            ));
        };
        if names.is_empty() {
            return Err(GitError::Command(
                "push --delete requires at least one ref".into(),
            ));
        }
        (
            remote.clone(),
            names.iter().map(|refspec| format!(":{refspec}")).collect(),
        )
    } else {
        push_remote_and_refspecs(&store, &positional)?
    };
    let options = PushOptions {
        quiet,
        set_upstream,
        force,
    };
    // All transports delegate the git work to `sley_remote::push`, picked purely
    // by the resolved `PushDestination`; this command keeps owning URL/repo
    // resolution, set-upstream config, and the "To <remote>" summary so the
    // user-visible output stays byte-for-byte identical.
    let destination = if push_remote_is_ssh(&remote)? {
        let remote_url = parse_remote_url(&push_resolved_url(&remote)?)?;
        sley_remote::PushDestination::Ssh(remote_url)
    } else if push_remote_is_http(&remote)? {
        let remote_url = parse_remote_url(&push_resolved_url(&remote)?)?;
        sley_remote::PushDestination::Http(remote_url)
    } else {
        let remote_git_dir = ls_remote_git_dir(&remote)?;
        let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
        sley_remote::PushDestination::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        }
    };
    run_push(
        &git_dir,
        &common_git_dir,
        format,
        &remote,
        &destination,
        &refspecs,
        options,
    )
}

#[derive(Debug, Clone, Copy)]
struct PushOptions {
    quiet: bool,
    set_upstream: bool,
    force: bool,
}

/// Drive [`sley_remote::push`] for an already-resolved `destination` (HTTP or
/// local), wiring the credential-helper provider and the stdout progress sink,
/// then reproduce the CLI's behavior from the structured outcome: nothing on a
/// no-op push, otherwise the optional set-upstream config write followed by the
/// "To <remote>" summary on stderr. Repository/URL resolution, the set-upstream
/// config, and output formatting stay here; the push orchestration lives in the
/// library.
fn run_push(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    remote: &str,
    destination: &sley_remote::PushDestination,
    refspecs: &[String],
    options: PushOptions,
) -> Result<()> {
    let config = read_repo_config(git_dir).unwrap_or_default();
    let mut credentials = sley_remote::CredentialHelperProvider::new(Some(&config));
    let mut progress = StdoutProgress;
    let remote_options = sley_remote::PushOptions {
        quiet: options.quiet,
        force: options.force,
    };
    let outcome = sley_remote::push(
        sley_remote::PushRequest {
            git_dir,
            common_git_dir,
            format,
            config: &config,
            remote,
            destination,
            refspecs,
            options: &remote_options,
        },
        sley_remote::PushServices {
            credentials: &mut credentials,
            progress: &mut progress,
        },
    )?;
    if outcome.commands.is_empty() {
        return Ok(());
    }
    if options.set_upstream {
        configure_push_upstreams(git_dir, remote, &outcome.commands)?;
    }
    if !options.quiet {
        eprintln!("To {remote}");
        for command in &outcome.commands {
            eprintln!("   {}  {}", command.new_id, command.name);
        }
    }
    Ok(())
}

fn push_remote_is_ssh(remote: &str) -> Result<bool> {
    let resolved = push_resolved_url(remote)?;
    Ok(parse_remote_url(&resolved)?.transport == RemoteTransport::Ssh)
}

fn push_resolved_url(remote: &str) -> Result<String> {
    if let Ok(git_dir) = discover_git_dir(&env::current_dir()?) {
        let config = read_repo_config(&git_dir)?;
        return Ok(resolve_remote_push_url(&config, remote));
    }
    Ok(remote.to_string())
}

fn push_remote_and_refspecs(
    store: &FileRefStore,
    positional: &[String],
) -> Result<(String, Vec<String>)> {
    match positional {
        [] => {
            let branch = store.current_branch()?.ok_or_else(|| {
                GitError::Command("push requires a refspec when HEAD is detached".into())
            })?;
            Ok(("origin".into(), vec![branch]))
        }
        [remote] => {
            let branch = store.current_branch()?.ok_or_else(|| {
                GitError::Command("push requires a refspec when HEAD is detached".into())
            })?;
            Ok((remote.clone(), vec![branch]))
        }
        [remote, refspecs @ ..] => Ok((remote.clone(), refspecs.to_vec())),
    }
}

fn configure_push_upstreams(
    git_dir: &Path,
    remote: &str,
    commands: &[ReceivePackCommand],
) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    for command in commands {
        let Some(branch) = command.name.strip_prefix("refs/heads/") else {
            continue;
        };
        let remote_key = ConfigKey {
            section: "branch".into(),
            subsection: Some(branch.to_string()),
            key: "remote".into(),
        };
        config_set_value(&mut config, &remote_key, remote, false);
        let merge_key = ConfigKey {
            section: "branch".into(),
            subsection: Some(branch.to_string()),
            key: "merge".into(),
        };
        config_set_value(&mut config, &merge_key, &command.name, false);
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
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<()> {
    let remote_git_dir = ls_remote_git_dir(source)?;
    let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
    let config = read_repo_config(git_dir)?;
    let fetch_source = sley_remote::FetchSource::Local {
        git_dir: remote_git_dir,
        common_git_dir: remote_common_git_dir,
    };
    run_fetch(
        git_dir,
        format,
        &config,
        source,
        &fetch_source,
        refspecs,
        options,
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
}

/// Drive [`sley_remote::fetch`] for an already-resolved `source`, wiring the
/// credential-helper provider and the stdout progress sink, then format the
/// outcome the way the CLI always has (prune notices are emitted through the sink
/// during the call; nothing else is printed for fetch).
fn run_fetch(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    source: &str,
    fetch_source: &sley_remote::FetchSource,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<()> {
    let mut credentials = sley_remote::CredentialHelperProvider::new(Some(config));
    let mut progress = StdoutProgress;
    sley_remote::fetch(
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
        },
    )?;
    Ok(())
}

pub(crate) fn fetch_source_is_ssh(source: &str) -> Result<bool> {
    let resolved = ls_remote_resolved_url(source)?;
    Ok(parse_remote_url(&resolved)?.transport == RemoteTransport::Ssh)
}

/// Resolve the repository context and delegate an SSH fetch to
/// [`sley_remote::fetch`] via the unified [`sley_remote::FetchSource::Ssh`]
/// dispatch. URL resolution and output formatting stay here; the fetch
/// orchestration (ref-map, pack install over `ssh`, `FETCH_HEAD`, prune) lives in
/// the library, shared with the HTTP and local transports.
pub(crate) fn fetch_ssh_repository(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<()> {
    let config = read_repo_config(git_dir)?;
    let remote = parse_remote_url(&ls_remote_resolved_url(source)?)?;
    let fetch_source = sley_remote::FetchSource::Ssh(remote);
    run_fetch(
        git_dir,
        format,
        &config,
        source,
        &fetch_source,
        refspecs,
        options,
    )
}

// ===== Transport dispatch =====
//
// The fetch/push/ls-remote git work for every transport (HTTP, SSH, local) lives
// in `sley_remote`, picked by the source enum (`FetchSource`/`PushDestination`/
// `LsRemoteSource`). These sniffers classify the resolved URL so the commands can
// build the right variant; URL/repo resolution, output formatting, and exit codes
// stay here.

fn fetch_source_is_http(source: &str) -> Result<bool> {
    sley_remote::remote_url_is_http(&ls_remote_resolved_url(source)?)
}

fn push_remote_is_http(remote: &str) -> Result<bool> {
    sley_remote::remote_url_is_http(&push_resolved_url(remote)?)
}

/// Resolve the repository context and delegate a smart-HTTP(S) fetch to
/// [`sley_remote::fetch`]. URL resolution and output formatting stay here; the
/// fetch orchestration lives in the library.
fn fetch_http_repository(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<()> {
    let config = read_repo_config(git_dir)?;
    let remote = parse_remote_url(&ls_remote_resolved_url(source)?)?;
    let fetch_source = sley_remote::FetchSource::Http(remote);
    run_fetch(
        git_dir,
        format,
        &config,
        source,
        &fetch_source,
        refspecs,
        options,
    )
}

/// Resolve `repository` to an HTTP(S) remote and list its advertisements via
/// [`sley_remote::ls_remote`], returning `None` for non-HTTP transports. URL/
/// config resolution and the ref-name pattern matching stay here; the
/// advertisement listing and class filtering live in the library.
fn ls_remote_http_records(
    repository: &str,
    options: &LsRemoteOptions,
) -> Result<Option<(Vec<LsRemoteRecord>, ObjectFormat)>> {
    let remote_url = ls_remote_resolved_url(repository)?;
    let parsed = parse_remote_url(&remote_url)?;
    if !matches!(
        parsed.transport,
        RemoteTransport::Http | RemoteTransport::Https
    ) {
        return Ok(None);
    }
    let config = discover_git_dir(env::current_dir()?)
        .ok()
        .and_then(|git_dir| read_repo_config(&git_dir).ok());
    let mut credentials = sley_remote::CredentialHelperProvider::new(config.as_ref());
    let records = sley_remote::ls_remote(
        &sley_remote::LsRemoteSource::Http(parsed),
        ObjectFormat::Sha1,
        &ls_remote_filter(options),
        &|name| ls_remote_ref_matches(name, &options.patterns),
        &mut credentials,
    )?;
    Ok(Some(records))
}

/// The library ref-class filter for the parsed ls-remote `options`.
fn ls_remote_filter(options: &LsRemoteOptions) -> sley_remote::LsRemoteFilter {
    sley_remote::LsRemoteFilter {
        heads: options.heads,
        tags: options.tags,
        refs_only: options.refs_only,
    }
}

#[derive(Debug, Default)]
struct LsRemoteOptions {
    heads: bool,
    tags: bool,
    refs_only: bool,
    symref: bool,
    exit_code: bool,
    get_url: bool,
    sort: Option<LsRemoteSort>,
    repository: Option<String>,
    patterns: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum LsRemoteSort {
    Refname,
    RefnameDescending,
    VersionRefname,
    VersionRefnameDescending,
    ObjectName,
    ObjectNameDescending,
    ObjectType,
    ObjectTypeDescending,
    ObjectSize,
    ObjectSizeDescending,
    ObjectSizeDisk,
    ObjectSizeDiskDescending,
    AuthorDate,
    AuthorDateDescending,
    CommitterDate,
    CommitterDateDescending,
    TaggerDate,
    TaggerDateDescending,
    CreatorDate,
    CreatorDateDescending,
}

pub(crate) fn cmd_ls_remote(args: &[String]) -> Result<()> {
    let options = parse_ls_remote_options(args)?;
    let repository = options.repository.as_deref().unwrap_or("origin");
    if options.get_url {
        println!("{}", ls_remote_display_url(repository)?);
        return Ok(());
    }
    let local_sort_git_dir = validate_ls_remote_sort_context(options.sort)?;
    let local_sort_format = local_sort_git_dir
        .as_deref()
        .map(repository_object_format)
        .transpose()?;

    if let Some((mut records, format)) = ls_remote_ssh_records(repository, &options)? {
        if options.exit_code && records.is_empty() {
            return Err(GitError::Exit(2));
        }
        sort_ls_remote_records(
            &mut records,
            options.sort,
            local_sort_git_dir.as_deref(),
            local_sort_format.unwrap_or(format),
        )?;
        for record in records {
            print_ls_remote_ref(&record, options.symref);
        }
        return Ok(());
    }

    if let Some((mut records, format)) = ls_remote_http_records(repository, &options)? {
        if options.exit_code && records.is_empty() {
            return Err(GitError::Exit(2));
        }
        sort_ls_remote_records(
            &mut records,
            options.sort,
            local_sort_git_dir.as_deref(),
            local_sort_format.unwrap_or(format),
        )?;
        for record in records {
            print_ls_remote_ref(&record, options.symref);
        }
        return Ok(());
    }

    let git_dir = ls_remote_git_dir(repository)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let (mut records, format) = sley_remote::ls_remote(
        &sley_remote::LsRemoteSource::Local { git_dir },
        format,
        &ls_remote_filter(&options),
        &|name| ls_remote_ref_matches(name, &options.patterns),
        &mut sley_remote::NoCredentials,
    )?;

    if options.exit_code && records.is_empty() {
        return Err(GitError::Exit(2));
    }
    sort_ls_remote_records(
        &mut records,
        options.sort,
        local_sort_git_dir.as_deref(),
        local_sort_format.unwrap_or(format),
    )?;
    for record in records {
        print_ls_remote_ref(&record, options.symref);
    }
    Ok(())
}

fn parse_ls_remote_options(args: &[String]) -> Result<LsRemoteOptions> {
    let mut options = LsRemoteOptions::default();
    let mut positional = Vec::new();
    let mut positional_only = false;
    let mut args = GitArgCursor::new(args);
    while let Some(arg) = args.next() {
        if positional_only {
            positional.push(arg.to_string());
            continue;
        }
        match arg {
            "--" => positional_only = true,
            "-b" | "--heads" | "--branches" => options.heads = true,
            "--no-heads" | "--no-branches" => options.heads = false,
            "-t" | "--tags" => options.tags = true,
            "--no-tags" => options.tags = false,
            "--refs" => options.refs_only = true,
            "--no-refs" => options.refs_only = false,
            "--symref" => options.symref = true,
            "--no-symref" => options.symref = false,
            "--exit-code" => options.exit_code = true,
            "--no-exit-code" => options.exit_code = false,
            "-q" | "--quiet" | "--no-quiet" => {}
            "--get-url" => options.get_url = true,
            "--no-get-url" => options.get_url = false,
            "--upload-pack" | "--server-option" | "-o" => {
                if args.next_value().is_none() {
                    return ls_remote_usage();
                }
            }
            "--sort" => {
                let Some(value) = args.next_value() else {
                    return ls_remote_usage();
                };
                options.sort = Some(parse_ls_remote_sort(value)?);
            }
            "--no-upload-pack" | "--no-server-option" => {}
            "--no-sort" => options.sort = None,
            value if long_option_value(value, "upload-pack").is_some() => {}
            value if long_option_value(value, "server-option").is_some() => {}
            value if let Some(sort) = long_option_value(value, "sort") => {
                options.sort = Some(parse_ls_remote_sort(sort)?);
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                return ls_remote_usage();
            }
            value => positional.push(value.to_string()),
        }
    }
    if let Some(repository) = positional.first() {
        options.repository = Some(repository.clone());
        options.patterns = positional[1..].to_vec();
    }
    Ok(options)
}

fn parse_ls_remote_sort(value: &str) -> Result<LsRemoteSort> {
    match value {
        "refname" => Ok(LsRemoteSort::Refname),
        "-refname" => Ok(LsRemoteSort::RefnameDescending),
        "version:refname" | "v:refname" => Ok(LsRemoteSort::VersionRefname),
        "-version:refname" | "-v:refname" => Ok(LsRemoteSort::VersionRefnameDescending),
        "objectname" => Ok(LsRemoteSort::ObjectName),
        "-objectname" => Ok(LsRemoteSort::ObjectNameDescending),
        "objecttype" => Ok(LsRemoteSort::ObjectType),
        "-objecttype" => Ok(LsRemoteSort::ObjectTypeDescending),
        "objectsize" => Ok(LsRemoteSort::ObjectSize),
        "-objectsize" => Ok(LsRemoteSort::ObjectSizeDescending),
        "objectsize:disk" => Ok(LsRemoteSort::ObjectSizeDisk),
        "-objectsize:disk" => Ok(LsRemoteSort::ObjectSizeDiskDescending),
        "authordate" => Ok(LsRemoteSort::AuthorDate),
        "-authordate" => Ok(LsRemoteSort::AuthorDateDescending),
        "committerdate" => Ok(LsRemoteSort::CommitterDate),
        "-committerdate" => Ok(LsRemoteSort::CommitterDateDescending),
        "taggerdate" => Ok(LsRemoteSort::TaggerDate),
        "-taggerdate" => Ok(LsRemoteSort::TaggerDateDescending),
        "creatordate" => Ok(LsRemoteSort::CreatorDate),
        "-creatordate" => Ok(LsRemoteSort::CreatorDateDescending),
        other => {
            eprintln!("fatal: unknown field name: {other}");
            Err(GitError::Exit(128))
        }
    }
}

fn validate_ls_remote_sort_context(sort: Option<LsRemoteSort>) -> Result<Option<PathBuf>> {
    if !matches!(
        sort,
        Some(
            LsRemoteSort::ObjectName
                | LsRemoteSort::ObjectNameDescending
                | LsRemoteSort::ObjectType
                | LsRemoteSort::ObjectTypeDescending
                | LsRemoteSort::ObjectSize
                | LsRemoteSort::ObjectSizeDescending
                | LsRemoteSort::ObjectSizeDisk
                | LsRemoteSort::ObjectSizeDiskDescending
                | LsRemoteSort::AuthorDate
                | LsRemoteSort::AuthorDateDescending
                | LsRemoteSort::CommitterDate
                | LsRemoteSort::CommitterDateDescending
                | LsRemoteSort::TaggerDate
                | LsRemoteSort::TaggerDateDescending
                | LsRemoteSort::CreatorDate
                | LsRemoteSort::CreatorDateDescending
        )
    ) {
        return Ok(None);
    }
    let field = match sort {
        Some(LsRemoteSort::ObjectName | LsRemoteSort::ObjectNameDescending) => "objectname",
        Some(LsRemoteSort::ObjectType | LsRemoteSort::ObjectTypeDescending) => "objecttype",
        Some(LsRemoteSort::ObjectSize | LsRemoteSort::ObjectSizeDescending) => "objectsize",
        Some(LsRemoteSort::ObjectSizeDisk | LsRemoteSort::ObjectSizeDiskDescending) => {
            "objectsize:disk"
        }
        Some(LsRemoteSort::AuthorDate | LsRemoteSort::AuthorDateDescending) => "authordate",
        Some(LsRemoteSort::CommitterDate | LsRemoteSort::CommitterDateDescending) => {
            "committerdate"
        }
        Some(LsRemoteSort::TaggerDate | LsRemoteSort::TaggerDateDescending) => "taggerdate",
        Some(LsRemoteSort::CreatorDate | LsRemoteSort::CreatorDateDescending) => "creatordate",
        _ => unreachable!("guard checked object-data sort"),
    };
    if let Ok(git_dir) = discover_git_dir(env::current_dir()?) {
        return Ok(Some(git_dir));
    }
    eprintln!(
        "fatal: not a git repository, but the field '{field}' requires access to object data"
    );
    Err(GitError::Exit(128))
}

/// Resolve `repository` to an SSH remote and list its advertisements via
/// [`sley_remote::ls_remote`], returning `None` for non-SSH transports. URL/config
/// resolution and the ref-name pattern matching stay here; the advertisement
/// listing and class filtering live in the library, shared with the HTTP path. SSH
/// does not authenticate at this layer, so no credential provider is supplied.
fn ls_remote_ssh_records(
    repository: &str,
    options: &LsRemoteOptions,
) -> Result<Option<(Vec<LsRemoteRecord>, ObjectFormat)>> {
    let parsed = parse_remote_url(&ls_remote_resolved_url(repository)?)?;
    if parsed.transport != RemoteTransport::Ssh {
        return Ok(None);
    }
    let records = sley_remote::ls_remote(
        &sley_remote::LsRemoteSource::Ssh(parsed),
        ObjectFormat::Sha1,
        &ls_remote_filter(options),
        &|name| ls_remote_ref_matches(name, &options.patterns),
        &mut sley_remote::NoCredentials,
    )?;
    Ok(Some(records))
}

fn ls_remote_resolved_url(repository: &str) -> Result<String> {
    let cwd = env::current_dir()?;
    if let Some(config) = discover_git_dir(&cwd)
        .ok()
        .and_then(|git_dir| read_repo_config(&git_dir).ok())
    {
        return Ok(resolve_remote_fetch_url(&config, repository));
    }
    Ok(repository.to_string())
}

pub(crate) fn ls_remote_git_dir(repository: &str) -> Result<PathBuf> {
    let cwd = env::current_dir()?;
    if let Ok(path) = ls_remote_repository_path(repository, &cwd)
        && path.exists()
        && let Ok(git_dir) = discover_git_dir(path)
    {
        return Ok(git_dir);
    }
    let local_git_dir = discover_git_dir(&cwd)?;
    let config = read_repo_config(&local_git_dir)?;
    let rewritten = rewrite_url_with_config(&config, repository, false);
    if rewritten != repository
        && let Ok(path) = ls_remote_repository_path(&rewritten, &cwd)
        && path.exists()
        && let Ok(git_dir) = discover_git_dir(path)
    {
        return Ok(git_dir);
    }
    local_remote_git_dir(&config, repository, &local_git_dir)
}

fn ls_remote_repository_path(repository: &str, cwd: &Path) -> Result<PathBuf> {
    let parsed = parse_remote_url(repository)?;
    match parsed.transport {
        RemoteTransport::Local => {
            let path = PathBuf::from(parsed.path);
            Ok(if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            })
        }
        RemoteTransport::File => Ok(PathBuf::from(percent_decode_url_path(&parsed.path)?)),
        RemoteTransport::Ssh
        | RemoteTransport::Git
        | RemoteTransport::Http
        | RemoteTransport::Https => Err(GitError::Unsupported(
            "ls-remote currently supports local repositories".into(),
        )),
    }
}

fn percent_decode_url_path(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(GitError::InvalidPath(format!(
                    "invalid percent-encoded path {value:?}"
                )));
            }
            let high = percent_hex_value(bytes[i + 1]).ok_or_else(|| {
                GitError::InvalidPath(format!("invalid percent-encoded path {value:?}"))
            })?;
            let low = percent_hex_value(bytes[i + 2]).ok_or_else(|| {
                GitError::InvalidPath(format!("invalid percent-encoded path {value:?}"))
            })?;
            decoded.push((high << 4) | low);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| GitError::InvalidPath(format!("invalid utf-8 file URL path {value:?}")))
}

fn percent_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn ls_remote_display_url(repository: &str) -> Result<String> {
    let cwd = env::current_dir()?;
    let config = discover_git_dir(&cwd)
        .ok()
        .and_then(|git_dir| read_repo_config(&git_dir).ok());
    let url = config
        .as_ref()
        .and_then(|config| {
            remote_config_values(config, repository, "url")
                .into_iter()
                .next()
        })
        .unwrap_or_else(|| repository.to_string());
    Ok(config
        .as_ref()
        .map(|config| rewrite_url_with_config(config, &url, false))
        .unwrap_or(url))
}

fn ls_remote_ref_matches(name: &str, patterns: &[String]) -> bool {
    patterns.is_empty()
        || patterns
            .iter()
            .any(|pattern| ls_remote_pattern_matches(name, pattern))
}

fn ls_remote_pattern_matches(name: &str, pattern: &str) -> bool {
    if !pattern
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
    {
        return show_ref_filter_matches(name, pattern);
    }
    name == pattern
        || refname_pattern_matches(pattern, name)
        || name
            .match_indices('/')
            .map(|(index, _)| &name[index + 1..])
            .any(|suffix| refname_pattern_matches(pattern, suffix))
}

fn sort_ls_remote_records(
    records: &mut [LsRemoteRecord],
    sort: Option<LsRemoteSort>,
    local_git_dir: Option<&Path>,
    format: ObjectFormat,
) -> Result<()> {
    let Some(sort) = sort else {
        return Ok(());
    };
    let local_db = if matches!(
        sort,
        LsRemoteSort::ObjectType
            | LsRemoteSort::ObjectTypeDescending
            | LsRemoteSort::ObjectSize
            | LsRemoteSort::ObjectSizeDescending
            | LsRemoteSort::ObjectSizeDisk
            | LsRemoteSort::ObjectSizeDiskDescending
            | LsRemoteSort::AuthorDate
            | LsRemoteSort::AuthorDateDescending
            | LsRemoteSort::CommitterDate
            | LsRemoteSort::CommitterDateDescending
            | LsRemoteSort::TaggerDate
            | LsRemoteSort::TaggerDateDescending
            | LsRemoteSort::CreatorDate
            | LsRemoteSort::CreatorDateDescending
    ) {
        Some(FileObjectDatabase::from_git_dir(
            local_git_dir.expect("object-data sort validated local git dir"),
            format,
        ))
    } else {
        None
    };
    let mut keyed = records
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, record)| {
            Ok((
                ls_remote_sort_key(&record, sort, local_db.as_ref(), local_git_dir)?,
                index,
                record,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    keyed.sort_by(|left, right| {
        let ordering = match sort {
            LsRemoteSort::Refname | LsRemoteSort::VersionRefname | LsRemoteSort::ObjectName => {
                left.0.cmp(&right.0)
            }
            LsRemoteSort::ObjectType | LsRemoteSort::ObjectSize => left.0.cmp(&right.0),
            LsRemoteSort::ObjectSizeDisk => left.0.cmp(&right.0),
            LsRemoteSort::AuthorDate
            | LsRemoteSort::CommitterDate
            | LsRemoteSort::TaggerDate
            | LsRemoteSort::CreatorDate => left.0.cmp(&right.0),
            LsRemoteSort::RefnameDescending
            | LsRemoteSort::VersionRefnameDescending
            | LsRemoteSort::ObjectNameDescending
            | LsRemoteSort::ObjectTypeDescending
            | LsRemoteSort::ObjectSizeDescending
            | LsRemoteSort::ObjectSizeDiskDescending
            | LsRemoteSort::AuthorDateDescending
            | LsRemoteSort::CommitterDateDescending
            | LsRemoteSort::TaggerDateDescending
            | LsRemoteSort::CreatorDateDescending => left.0.cmp(&right.0).reverse(),
        };
        ordering.then_with(|| left.1.cmp(&right.1))
    });
    for (destination, (_, _, record)) in records.iter_mut().zip(keyed) {
        *destination = record;
    }
    Ok(())
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum LsRemoteSortKey {
    Number(i128),
    Text(String),
    Version(String),
}

impl Ord for LsRemoteSortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (LsRemoteSortKey::Number(left), LsRemoteSortKey::Number(right)) => left.cmp(right),
            (LsRemoteSortKey::Text(left), LsRemoteSortKey::Text(right)) => left.cmp(right),
            (LsRemoteSortKey::Version(left), LsRemoteSortKey::Version(right)) => {
                version_sort_cmp(left, right, &[])
            }
            (left, right) => ls_remote_sort_key_rank(left).cmp(&ls_remote_sort_key_rank(right)),
        }
    }
}

impl PartialOrd for LsRemoteSortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn ls_remote_sort_key_rank(key: &LsRemoteSortKey) -> u8 {
    match key {
        LsRemoteSortKey::Number(_) => 0,
        LsRemoteSortKey::Text(_) => 1,
        LsRemoteSortKey::Version(_) => 2,
    }
}

fn ls_remote_sort_key(
    record: &LsRemoteRecord,
    sort: LsRemoteSort,
    local_db: Option<&FileObjectDatabase>,
    local_git_dir: Option<&Path>,
) -> Result<LsRemoteSortKey> {
    match sort {
        LsRemoteSort::Refname | LsRemoteSort::RefnameDescending => {
            Ok(LsRemoteSortKey::Text(record.name.clone()))
        }
        LsRemoteSort::VersionRefname | LsRemoteSort::VersionRefnameDescending => {
            Ok(LsRemoteSortKey::Version(record.name.clone()))
        }
        LsRemoteSort::ObjectName | LsRemoteSort::ObjectNameDescending => {
            Ok(LsRemoteSortKey::Text(record.oid.to_hex()))
        }
        LsRemoteSort::ObjectType | LsRemoteSort::ObjectTypeDescending => {
            let db = local_db.expect("objecttype sort requires local db");
            let object = db.read_object(&record.oid).map_err(|_| {
                eprintln!("fatal: missing object {} for {}", record.oid, record.name);
                GitError::Exit(128)
            })?;
            Ok(LsRemoteSortKey::Text(
                object.object_type.as_str().to_string(),
            ))
        }
        LsRemoteSort::ObjectSize | LsRemoteSort::ObjectSizeDescending => {
            let db = local_db.expect("objectsize sort requires local db");
            let object = db.read_object(&record.oid).map_err(|_| {
                eprintln!("fatal: missing object {} for {}", record.oid, record.name);
                GitError::Exit(128)
            })?;
            Ok(LsRemoteSortKey::Number(object.body.len() as i128))
        }
        LsRemoteSort::ObjectSizeDisk | LsRemoteSort::ObjectSizeDiskDescending => {
            let git_dir = local_git_dir.expect("objectsize:disk sort requires local git dir");
            let storage = cat_file_object_storage(git_dir, record.oid.format(), &record.oid)
                .map_err(|_| {
                    eprintln!("fatal: missing object {} for {}", record.oid, record.name);
                    GitError::Exit(128)
                })?;
            Ok(LsRemoteSortKey::Number(storage.disk_size as i128))
        }
        LsRemoteSort::AuthorDate | LsRemoteSort::AuthorDateDescending => {
            ls_remote_date_sort_key(record, local_db, ForEachRefDateSortField::Author)
        }
        LsRemoteSort::CommitterDate | LsRemoteSort::CommitterDateDescending => {
            ls_remote_date_sort_key(record, local_db, ForEachRefDateSortField::Committer)
        }
        LsRemoteSort::TaggerDate | LsRemoteSort::TaggerDateDescending => {
            ls_remote_date_sort_key(record, local_db, ForEachRefDateSortField::Tagger)
        }
        LsRemoteSort::CreatorDate | LsRemoteSort::CreatorDateDescending => {
            ls_remote_date_sort_key(record, local_db, ForEachRefDateSortField::Creator)
        }
    }
}

fn ls_remote_date_sort_key(
    record: &LsRemoteRecord,
    local_db: Option<&FileObjectDatabase>,
    field: ForEachRefDateSortField,
) -> Result<LsRemoteSortKey> {
    let db = local_db.expect("date sort requires local db");
    let object = db.read_object(&record.oid).map_err(|_| {
        eprintln!("fatal: missing object {} for {}", record.oid, record.name);
        GitError::Exit(128)
    })?;
    let contents = for_each_ref_contents(record.oid.format(), &object)?;
    Ok(LsRemoteSortKey::Number(for_each_ref_sort_date_key(
        contents, field,
    )))
}

fn print_ls_remote_ref(record: &LsRemoteRecord, show_symref: bool) {
    if show_symref && let Some(symref) = &record.symref {
        println!("ref: {symref}\t{}", record.name);
    }
    println!("{}\t{}", record.oid, record.name);
}

fn ls_remote_usage<T>() -> Result<T> {
    eprintln!("usage: git ls-remote [--branches] [--tags] [--refs] [--upload-pack=<exec>]");
    eprintln!("                     [-q | --quiet] [--exit-code] [--get-url] [--sort=<key>]");
    eprintln!("                     [--symref] [<repository> [<patterns>...]]");
    eprintln!();
    eprintln!("    -q, --[no-]quiet      do not print remote URL");
    eprintln!("    --[no-]upload-pack <exec>");
    eprintln!("                          path of git-upload-pack on the remote host");
    eprintln!("    -t, --[no-]tags       limit to tags");
    eprintln!("    -b, --[no-]branches   limit to branches");
    eprintln!("    --[no-]refs           do not show peeled tags");
    eprintln!("    --[no-]get-url        take url.<base>.insteadOf into account");
    eprintln!("    --[no-]sort <key>     field name to sort on");
    eprintln!("    --[no-]exit-code      exit with exit code 2 if no matching refs are found");
    eprintln!(
        "    --[no-]symref         show underlying ref in addition to the object pointed by it"
    );
    eprintln!("    -o, --[no-]server-option <server-specific>");
    eprintln!("                          option to transmit");
    eprintln!();
    Err(GitError::Exit(129))
}
pub(crate) fn cmd_remote(args: &[String]) -> Result<()> {
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
        return remote_list(verbose);
    }
    match args[idx].as_str() {
        "add" => cmd_remote_add(&args[idx + 1..]),
        "get-url" => cmd_remote_get_url(&args[idx + 1..]),
        "prune" => cmd_remote_prune(&args[idx + 1..]),
        "rename" => cmd_remote_rename(&args[idx + 1..]),
        "remove" | "rm" => cmd_remote_remove(&args[idx + 1..]),
        "set-branches" => cmd_remote_set_branches(&args[idx + 1..]),
        "set-head" => cmd_remote_set_head(&args[idx + 1..]),
        "set-url" => cmd_remote_set_url(&args[idx + 1..]),
        "show" => cmd_remote_show(&args[idx + 1..]),
        other => Err(GitError::Command(format!(
            "unsupported remote subcommand {other}"
        ))),
    }
}
fn remote_list(verbose: bool) -> Result<()> {
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let config = read_repo_config(&git_dir)?;
    let mut stdout = io::stdout();
    for name in remote_names(&config) {
        if verbose {
            if let Some(url) = config.get("remote", Some(&name), "url") {
                let fetch_url = rewrite_url_with_config(&config, url, false);
                let push_url = config.get("remote", Some(&name), "pushurl").unwrap_or(url);
                let push_url = rewrite_url_with_config(&config, push_url, true);
                writeln!(stdout, "{name}\t{fetch_url} (fetch)")?;
                writeln!(stdout, "{name}\t{push_url} (push)")?;
            }
        } else {
            writeln!(stdout, "{name}")?;
        }
    }
    Ok(())
}

pub(crate) fn cmd_remote_add(args: &[String]) -> Result<()> {
    let mut branches = Vec::new();
    let mut master = None;
    let mut tag_opt = None;
    let mut mirror = RemoteAddMirror::None;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
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
        return Err(GitError::Command(
            "remote add requires [-t <branch>] <name> <url>".into(),
        ));
    }
    let name = positional[0];
    let url = positional[1];
    validate_remote_name(name)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let mut config = read_repo_config(&git_dir)?;
    // Build the section body from the parsed options, then let the shared editor
    // append it (and reject a duplicate remote).
    let mut entries = vec![ConfigEntry::new("url", Some(url.to_string()))];
    match mirror {
        RemoteAddMirror::Fetch | RemoteAddMirror::Both => {
            entries.push(ConfigEntry::new("fetch", Some("+refs/*:refs/*".into())));
        }
        RemoteAddMirror::Push => {
            entries.push(ConfigEntry::new("mirror", Some("true".into())));
        }
        RemoteAddMirror::None => {
            if branches.is_empty() {
                entries.push(ConfigEntry::new(
                    "fetch",
                    Some(sley_config::remotes::default_fetch_refspec(name)),
                ));
            } else {
                for branch in &branches {
                    entries.push(ConfigEntry::new(
                        "fetch",
                        Some(remote_branch_fetch_refspec(name, branch)),
                    ));
                }
            }
        }
    }
    if let Some(tag_opt) = tag_opt {
        entries.push(ConfigEntry::new("tagopt", Some(tag_opt)));
    }
    if mirror == RemoteAddMirror::Both {
        entries.push(ConfigEntry::new("mirror", Some("true".into())));
    }
    match sley_config::remotes::add_remote(&mut config, name, entries) {
        Ok(()) => {}
        Err(sley_config::remotes::RemoteEditError::AlreadyExists) => {
            return Err(GitError::Command(format!("remote {name} already exists")));
        }
        Err(sley_config::remotes::RemoteEditError::NotFound) => {
            return Err(GitError::remote_not_found(name));
        }
    }
    write_repo_config(&git_dir, &config)?;
    if let Some(master) = master {
        let format = repository_object_format(&git_dir)?;
        let store = FileRefStore::new(&git_dir, format);
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: format!("refs/remotes/{name}/HEAD"),
            expected: None,
            new: RefTarget::Symbolic(format!("refs/remotes/{name}/{master}")),
            reflog: None,
        });
        tx.commit()?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteAddMirror {
    None,
    Fetch,
    Push,
    Both,
}

fn parse_remote_add_mirror(value: &str) -> Result<RemoteAddMirror> {
    match value {
        "fetch" => Ok(RemoteAddMirror::Fetch),
        "push" => Ok(RemoteAddMirror::Push),
        _ => Err(GitError::Command(format!(
            "remote add --mirror expects fetch or push, got {value}"
        ))),
    }
}

pub(crate) fn cmd_remote_get_url(args: &[String]) -> Result<()> {
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
        return Err(GitError::Command(
            "remote get-url requires [--push] [--all] <name>".into(),
        ));
    }
    let name = positional[0];
    validate_remote_name(name)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let config = read_repo_config(&git_dir)?;
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

pub(crate) fn cmd_remote_remove(args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(GitError::Command("remote remove requires <name>".into()));
    }
    let name = &args[0];
    validate_remote_name(name)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let mut config = read_repo_config(&git_dir)?;
    match sley_config::remotes::remove_remote(&mut config, name) {
        Ok(()) => {}
        Err(sley_config::remotes::RemoteEditError::NotFound) => {
            return Err(GitError::remote_not_found(name));
        }
        Err(sley_config::remotes::RemoteEditError::AlreadyExists) => {
            return Err(GitError::Command(format!("remote {name} already exists")));
        }
    }
    write_repo_config(&git_dir, &config)?;
    let format = repository_object_format(&git_dir)?;
    remove_remote_tracking_refs(&git_dir, format, name)
}

pub(crate) fn cmd_remote_prune(args: &[String]) -> Result<()> {
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
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let config = read_repo_config(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let mut stdout = io::stdout();
    for name in names {
        validate_remote_name(name)?;
        prune_remote_tracking_refs(&mut stdout, &config, &store, &git_dir, name, dry_run)?;
    }
    Ok(())
}

pub(crate) fn cmd_remote_rename(args: &[String]) -> Result<()> {
    if args.len() != 2 {
        return Err(GitError::Command(
            "remote rename requires <old> <new>".into(),
        ));
    }
    let old = &args[0];
    let new = &args[1];
    validate_remote_name(old)?;
    validate_remote_name(new)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let mut config = read_repo_config(&git_dir)?;
    if config
        .sections
        .iter()
        .any(|section| section.name == "remote" && section.subsection.as_deref() == Some(new))
    {
        return Err(GitError::Command(format!("remote {new} already exists")));
    }
    let mut renamed = false;
    for section in &mut config.sections {
        if section.name == "remote" && section.subsection.as_deref() == Some(old) {
            section.subsection = Some(new.to_string());
            for entry in &mut section.entries {
                if entry.key.eq_ignore_ascii_case("fetch")
                    && let Some(value) = &mut entry.value
                {
                    *value = value.replace(
                        &format!("refs/remotes/{old}/"),
                        &format!("refs/remotes/{new}/"),
                    );
                }
            }
            move_remote_fetch_entries_to_end(section);
            renamed = true;
        } else if section.name == "branch" {
            for entry in &mut section.entries {
                if (entry.key.eq_ignore_ascii_case("remote")
                    || entry.key.eq_ignore_ascii_case("pushRemote"))
                    && entry.value.as_deref() == Some(old)
                {
                    entry.value = Some(new.to_string());
                }
            }
        } else if section.name == "remote" && section.subsection.is_none() {
            for entry in &mut section.entries {
                if entry.key.eq_ignore_ascii_case("pushDefault")
                    && entry.value.as_deref() == Some(old)
                {
                    entry.value = Some(new.to_string());
                }
            }
        }
    }
    if !renamed {
        return Err(GitError::remote_not_found(old));
    }
    write_repo_config(&git_dir, &config)?;
    let format = repository_object_format(&git_dir)?;
    rename_remote_tracking_refs(&git_dir, format, old, new)
}

fn move_remote_fetch_entries_to_end(section: &mut ConfigSection) {
    let mut fetch_entries = Vec::new();
    let entries = mem::take(&mut section.entries);
    for entry in entries {
        if entry.key.eq_ignore_ascii_case("fetch") {
            fetch_entries.push(entry);
        } else {
            section.entries.push(entry);
        }
    }
    section.entries.extend(fetch_entries);
}

fn remove_remote_tracking_refs(git_dir: &Path, format: ObjectFormat, remote: &str) -> Result<()> {
    let prefix = format!("refs/remotes/{remote}/");
    remove_remote_packed_refs(git_dir, format, &prefix)?;
    remove_remote_ref_dir(git_dir, "refs", remote)?;
    remove_remote_ref_dir(git_dir, "logs/refs", remote)
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
    for reference in refs {
        let Some(suffix) = reference.name.strip_prefix(&old_prefix) else {
            continue;
        };
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
            name: format!("{new_prefix}{suffix}"),
            expected: None,
            new: target,
            reflog: None,
        });
    }
    tx.commit()?;
    remove_remote_packed_refs(git_dir, format, &old_prefix)?;
    remove_remote_ref_dir(git_dir, "refs", old)?;
    rename_remote_ref_dir(git_dir, "logs/refs", old, new)?;
    Ok(())
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
    let remote_branches = branch_names_with_prefix(&remote_refs, "refs/heads/")
        .into_iter()
        .collect::<BTreeSet<_>>();
    let local_refs = store.list_refs()?;
    let stale_branches = remote_tracking_branch_names(&local_refs, remote)
        .into_iter()
        .filter(|branch| !remote_branches.contains(branch))
        .collect::<Vec<_>>();
    if stale_branches.is_empty() {
        return Ok(());
    }
    let display_url = remote_config_values(config, remote, "url")
        .into_iter()
        .next()
        .unwrap_or_else(|| remote.into());
    writeln!(stdout, "Pruning {remote}")?;
    writeln!(stdout, "URL: {display_url}")?;
    let remote_head = format!("refs/remotes/{remote}/HEAD");
    let remote_prefix = format!("refs/remotes/{remote}/");
    let head_target = match store.read_ref(&remote_head)? {
        Some(RefTarget::Symbolic(target)) => Some(target),
        Some(RefTarget::Direct(_)) | None => None,
    };
    for branch in stale_branches {
        let refname = format!("{remote_prefix}{branch}");
        if dry_run {
            writeln!(stdout, " * [would prune] {remote}/{branch}")?;
            if head_target.as_deref() == Some(refname.as_str()) {
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
        writeln!(stdout, " * [pruned] {remote}/{branch}")?;
        if head_target.as_deref() == Some(refname.as_str()) {
            let _ = store.delete_symbolic_ref(&remote_head)?;
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
    if path.exists() {
        fs::remove_dir_all(path)?;
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

pub(crate) fn cmd_remote_set_branches(args: &[String]) -> Result<()> {
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
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let mut config = read_repo_config(&git_dir)?;
    let Some(section) =
        config.sections.iter_mut().rev().find(|section| {
            section.name == "remote" && section.subsection.as_deref() == Some(name)
        })
    else {
        return Err(GitError::remote_not_found(name));
    };
    if !add {
        section
            .entries
            .retain(|entry| !entry.key.eq_ignore_ascii_case("fetch"));
    }
    for branch in branches {
        section.entries.push(ConfigEntry::new(
            "fetch",
            Some(remote_branch_fetch_refspec(name, branch)),
        ));
    }
    write_repo_config(&git_dir, &config)
}

pub(crate) fn cmd_remote_set_head(args: &[String]) -> Result<()> {
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
            return Err(GitError::Command(
                "remote set-head requires <name> (-d|--delete|<branch>)".into(),
            ));
        }
    };
    validate_remote_name(name)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let config = read_repo_config(&git_dir)?;
    if !remote_exists(&config, name) {
        return Err(GitError::remote_not_found(name));
    }
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let head = format!("refs/remotes/{name}/HEAD");
    if action == RemoteSetHeadAction::Delete {
        let _ = store.delete_symbolic_ref(&head)?;
        return Ok(());
    }
    if action == RemoteSetHeadAction::Auto {
        let branch = discover_local_remote_head_branch(&config, name, &git_dir)?;
        validate_remote_branch_name(&branch)?;
        let target = format!("refs/remotes/{name}/{branch}");
        if store.read_ref(&target)?.is_none() {
            return Err(GitError::Command(format!("Not a valid ref: {target}")));
        }
        let old_branch = match store.read_ref(&head)? {
            Some(RefTarget::Symbolic(target)) => target
                .strip_prefix(&format!("refs/remotes/{name}/"))
                .map(str::to_string),
            _ => None,
        };
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: head,
            expected: None,
            new: RefTarget::Symbolic(target),
            reflog: None,
        });
        tx.commit()?;
        match old_branch.as_deref() {
            Some(old) if old == branch => {
                println!("'{name}/HEAD' is unchanged and points to '{branch}'");
            }
            Some(old) => {
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
        return Err(GitError::reference_not_found(format!(
            "remote ref {target}"
        )));
    }
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: head,
        expected: None,
        new: RefTarget::Symbolic(target),
        reflog: None,
    });
    tx.commit()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteSetHeadAction {
    Branch,
    Delete,
    Auto,
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
        Some(RefTarget::Symbolic(target)) => target
            .strip_prefix("refs/heads/")
            .map(str::to_string)
            .ok_or_else(|| GitError::reference_not_found("remote HEAD branch")),
        Some(RefTarget::Direct(_)) | None => {
            Err(GitError::reference_not_found("remote HEAD branch"))
        }
    }
}

fn local_remote_git_dir(config: &GitConfig, name: &str, git_dir: &Path) -> Result<PathBuf> {
    let url = remote_config_values(config, name, "url")
        .into_iter()
        .next()
        .ok_or_else(|| GitError::not_found(format!("remote {name} url")))?;
    let url = rewrite_url_with_config(config, &url, false);
    let parsed = parse_remote_url(&url)?;
    let remote_path = match parsed.transport {
        RemoteTransport::Local => {
            let path = PathBuf::from(parsed.path);
            if path.is_absolute() {
                path
            } else {
                repository_relative_path_base(git_dir)?.join(path)
            }
        }
        RemoteTransport::File => PathBuf::from(percent_decode_url_path(&parsed.path)?),
        RemoteTransport::Ssh
        | RemoteTransport::Git
        | RemoteTransport::Http
        | RemoteTransport::Https => {
            return Err(GitError::Unsupported(
                "remote discovery for non-local transports".into(),
            ));
        }
    };
    discover_git_dir(remote_path)
}

fn repository_relative_path_base(git_dir: &Path) -> Result<PathBuf> {
    if git_dir.file_name().is_some_and(|name| name == ".git") {
        return git_dir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| GitError::InvalidPath("git dir has no parent".into()));
    }
    env::current_dir().map_err(GitError::from)
}

pub(crate) fn cmd_remote_set_url(args: &[String]) -> Result<()> {
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
        return Err(GitError::Command(
            "remote set-url requires [--push] [--add|--delete] <name> <url> [<oldurl>]".into(),
        ));
    }
    let name = positional[0];
    let url = positional[1];
    let old_url = positional.get(2).copied();
    validate_remote_name(name)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let mut config = read_repo_config(&git_dir)?;
    let kind = if push {
        sley_config::remotes::SetUrlKind::Push
    } else {
        sley_config::remotes::SetUrlKind::Fetch
    };
    let key = kind.key();
    // `--delete`/`<oldurl>` select URLs with git's value-pattern matcher; build
    // it here (the regex lives in the CLI) and hand the predicate to the editor.
    let delete_matcher = delete.then(|| SimpleConfigRegex::parse(url));
    let old_url_matcher = old_url.map(SimpleConfigRegex::parse);
    let op = if add {
        sley_config::remotes::SetUrlOp::Add { url }
    } else if let Some(matcher) = &delete_matcher {
        sley_config::remotes::SetUrlOp::Delete {
            matches: &|value| matcher.is_match(value),
        }
    } else if let Some(matcher) = &old_url_matcher {
        sley_config::remotes::SetUrlOp::Replace {
            url,
            matches: &|value| matcher.is_match(value),
        }
    } else {
        sley_config::remotes::SetUrlOp::Set { url }
    };
    match sley_config::remotes::set_url(&mut config, name, kind, op) {
        Ok(()) => write_repo_config(&git_dir, &config),
        Err(sley_config::remotes::SetUrlError::RemoteNotFound) => {
            Err(GitError::remote_not_found(name))
        }
        Err(sley_config::remotes::SetUrlError::NoMatch) => {
            // Only reachable for the `<oldurl>` (replace) form.
            remote_set_url_no_match(old_url.unwrap_or(url))
        }
        Err(sley_config::remotes::SetUrlError::DeleteNoMatch) => {
            remote_set_url_delete_no_match(name, key)
        }
        Err(sley_config::remotes::SetUrlError::DeleteAllFetchUrls) => {
            remote_set_url_delete_all_fetch_urls()
        }
        Err(sley_config::remotes::SetUrlError::MultipleValues) => {
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

pub(crate) fn cmd_remote_show(args: &[String]) -> Result<()> {
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
        return remote_list(false);
    }
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let config = read_repo_config(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let refs = store.list_refs()?;
    let mut stdout = io::stdout();
    for name in names {
        validate_remote_name(name)?;
        if no_query {
            write_remote_show_no_query(&mut stdout, &config, &refs, name)?;
        } else {
            write_remote_show_query(&mut stdout, &config, &refs, name, &git_dir)?;
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
    let fetch_urls = remote_config_values(config, name, "url");
    let push_urls = remote_config_values(config, name, "pushurl");
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
        writeln!(stdout, "  Push  URL: {display_url}")?;
    } else {
        for url in push_urls {
            writeln!(stdout, "  Push  URL: {url}")?;
        }
    }
    writeln!(stdout, "  HEAD branch: {remote_head_branch}")?;

    let remote_branches = branch_names_with_prefix(&remote_refs, "refs/heads/");
    let local_branches = remote_tracking_branch_names(refs, name);
    let local_branch_set = local_branches.iter().cloned().collect::<BTreeSet<_>>();
    let remote_branch_set = remote_branches.iter().cloned().collect::<BTreeSet<_>>();
    let mut branch_rows = Vec::new();
    for branch in &remote_branches {
        let status = if local_branch_set.contains(branch) {
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
        let local_db = FileObjectDatabase::from_git_dir(git_dir, remote_format);
        write_remote_show_push_config(
            stdout,
            &pull_branches,
            refs,
            &remote_refs,
            &local_db,
            remote_format,
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
    let fetch_urls = remote_config_values(config, name, "url");
    let push_urls = remote_config_values(config, name, "pushurl");
    let display_url = fetch_urls.first().map(String::as_str).unwrap_or(name);
    writeln!(stdout, "* remote {name}")?;
    writeln!(stdout, "  Fetch URL: {display_url}")?;
    if push_urls.is_empty() {
        writeln!(stdout, "  Push  URL: {display_url}")?;
    } else {
        for url in push_urls {
            writeln!(stdout, "  Push  URL: {url}")?;
        }
    }
    writeln!(stdout, "  HEAD branch: (not queried)")?;
    let remote_branches = remote_tracking_branch_names(refs, name);
    if !remote_branches.is_empty() {
        writeln!(stdout, "  Remote branches: (status not queried)")?;
        for branch in remote_branches {
            writeln!(stdout, "    {branch}")?;
        }
    }
    let pull_branches = remote_pull_branch_configs(config, name);
    if !pull_branches.is_empty() {
        write_remote_show_pull_config(stdout, &pull_branches)?;
    }
    writeln!(
        stdout,
        "  Local ref configured for 'git push' (status not queried):"
    )?;
    writeln!(stdout, "    (matching) pushes to (matching)")?;
    Ok(())
}

fn write_remote_show_pull_config(
    stdout: &mut impl Write,
    pull_branches: &[(String, String)],
) -> Result<()> {
    if pull_branches.len() == 1 {
        writeln!(stdout, "  Local branch configured for 'git pull':")?;
    } else {
        writeln!(stdout, "  Local branches configured for 'git pull':")?;
    }
    let width = pull_branches
        .iter()
        .map(|(branch, _)| branch.len())
        .max()
        .unwrap_or(0)
        + 1;
    for (branch, merge) in pull_branches {
        writeln!(
            stdout,
            "    {branch:<width$}merges with remote {merge}",
            width = width
        )?;
    }
    Ok(())
}

fn write_remote_show_push_config(
    stdout: &mut impl Write,
    branches: &[(String, String)],
    local_refs: &[sley_refs::Ref],
    remote_refs: &[sley_refs::Ref],
    local_db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<()> {
    if branches.len() == 1 {
        writeln!(stdout, "  Local ref configured for 'git push':")?;
    } else {
        writeln!(stdout, "  Local refs configured for 'git push':")?;
    }
    let local_width = branches
        .iter()
        .map(|(branch, _)| branch.len())
        .max()
        .unwrap_or(0)
        + 1;
    let remote_width = branches
        .iter()
        .map(|(_, merge)| merge.len())
        .max()
        .unwrap_or(0)
        + 1;
    for (branch, merge) in branches {
        let status =
            remote_show_push_status(branch, merge, local_refs, remote_refs, local_db, format);
        writeln!(
            stdout,
            "    {branch:<local_width$}pushes to {merge:<remote_width$}({status})",
            local_width = local_width,
            remote_width = remote_width,
        )?;
    }
    Ok(())
}

fn remote_show_push_status(
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
        return "local out of date";
    };
    if local_oid == remote_oid {
        return "up to date";
    }
    match ancestor_depths(local_db, format, local_oid) {
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

fn remote_pull_branch_configs(config: &GitConfig, remote: &str) -> Vec<(String, String)> {
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
        let Some(merge) = section
            .entries
            .iter()
            .find(|entry| entry.key.eq_ignore_ascii_case("merge"))
            .and_then(|entry| entry.value.as_deref())
        else {
            continue;
        };
        let merge = merge.strip_prefix("refs/heads/").unwrap_or(merge);
        branches.push((branch.to_string(), merge.to_string()));
    }
    branches.sort_by(|left, right| left.0.cmp(&right.0));
    branches
}

pub(crate) fn read_repo_config(git_dir: &Path) -> Result<GitConfig> {
    let path = git_dir.join("config");
    // Resolve `include.path` / `includeIf` directives. With no includes present
    // this yields the same config as a plain parse, and a missing file yields an
    // empty config — preserving prior behavior.
    let git_dir_abs = fs::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
    let context = sley_config::ConfigIncludeContext::new(
        Some(git_dir_abs),
        repo_current_branch_name(git_dir),
    );
    sley_config::load_config_with_includes(&path, &context)
}

/// Short branch name from `HEAD` (e.g. "main"), or None when detached/unborn.
/// Used for `includeIf "onbranch:<glob>"` resolution; reads HEAD directly so it
/// needs no object-format or ref-store context.
pub(crate) fn repo_current_branch_name(git_dir: &Path) -> Option<String> {
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let target = head.trim().strip_prefix("ref:")?.trim();
    target
        .strip_prefix("refs/heads/")
        .map(|name| name.to_string())
}

pub(crate) fn write_repo_config(git_dir: &Path, config: &GitConfig) -> Result<()> {
    fs::write(git_dir.join("config"), config.to_canonical_bytes())?;
    Ok(())
}

pub(crate) fn remote_names(config: &GitConfig) -> Vec<String> {
    sley_config::remotes::remote_names(config)
}

pub(crate) fn remote_exists(config: &GitConfig, name: &str) -> bool {
    sley_config::remotes::remote_exists(config, name)
}

fn remote_branch_fetch_refspec(remote: &str, branch: &str) -> String {
    format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}")
}

pub(crate) fn validate_remote_name(name: &str) -> Result<()> {
    if name.is_empty() || name.starts_with('-') {
        return Err(GitError::InvalidFormat("remote name is invalid".into()));
    }
    if name.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "remote name contains a delimiter byte".into(),
        ));
    }
    Ok(())
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
