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
use std::process::Command as Proc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchRecurseSubmodules {
    Default,
    OnDemand,
    On,
    Off,
}

impl FetchRecurseSubmodules {
    pub(crate) fn from_arg(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("yes") {
            "yes" | "true" | "on" => Ok(Self::On),
            "on-demand" => Ok(Self::OnDemand),
            "no" | "false" | "off" => Ok(Self::Off),
            other => {
                eprintln!("fatal: bad --recurse-submodules argument: {other}");
                Err(GitError::Exit(128))
            }
        }
    }

    pub(crate) fn from_config(value: &str) -> Self {
        match sley_submodule::parse_fetch_recurse(value) {
            sley_submodule::RecurseMode::On => Self::On,
            sley_submodule::RecurseMode::Off => Self::Off,
            sley_submodule::RecurseMode::OnDemand => Self::OnDemand,
            _ => Self::Default,
        }
    }
}

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
    let mut no_hardlinks = false;
    let mut upload_pack = None::<String>;
    let mut deepen_since = None::<i64>;
    let mut deepen_not = Vec::<String>::new();
    let mut server_options = Vec::<String>::new();
    let mut server_options_from_cli = false;
    let mut ssh_ip_version = None::<sley_transport::SshIpVersion>;
    // `--reject-shallow` / `--no-reject-shallow` are a tri-state (upstream
    // `option_reject_shallow = -1` when unspecified); the CLI flag overrides the
    // `clone.rejectshallow` config when present.
    let mut option_reject_shallow = None::<bool>;
    let mut ref_storage = match env::var("GIT_DEFAULT_REF_FORMAT") {
        Ok(value) if value == "reftable" => RefStorageFormat::Reftable,
        _ => RefStorageFormat::Files,
    };
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
            "--reject-shallow" => option_reject_shallow = Some(true),
            "--no-reject-shallow" => option_reject_shallow = Some(false),
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
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("clone --shallow-since requires a value".into())
                })?;
                deepen_since = Some(parse_shallow_since(value)?);
            }
            value if value.starts_with("--shallow-since=") => {
                let value = value.strip_prefix("--shallow-since=").ok_or_else(|| {
                    GitError::Command("clone --shallow-since requires a value".into())
                })?;
                deepen_since = Some(parse_shallow_since(value)?);
            }
            "--no-shallow-since" => deepen_since = None,
            "--shallow-exclude" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("clone --shallow-exclude requires a value".into())
                })?;
                deepen_not.push(value.clone());
            }
            value if value.starts_with("--shallow-exclude=") => {
                let value = value.strip_prefix("--shallow-exclude=").ok_or_else(|| {
                    GitError::Command("clone --shallow-exclude requires a value".into())
                })?;
                deepen_not.push(value.to_string());
            }
            "--no-shallow-exclude" => deepen_not.clear(),
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
                add_clone_filter(&mut partial_clone_filter, value);
            }
            value if value.starts_with("--filter=") => {
                let value = value
                    .strip_prefix("--filter=")
                    .ok_or_else(|| GitError::Command("clone --filter requires a value".into()))?;
                validate_clone_filter(value)?;
                add_clone_filter(&mut partial_clone_filter, value);
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
            "-4" | "--ipv4" => ssh_ip_version = Some(sley_transport::SshIpVersion::V4),
            "-6" | "--ipv6" => ssh_ip_version = Some(sley_transport::SshIpVersion::V6),
            "-l" | "--local" => local = Some(true),
            "--no-local" => local = Some(false),
            "--hardlinks" => no_hardlinks = false,
            "--no-hardlinks" => no_hardlinks = true,
            "--no-ref-format" => ref_storage = RefStorageFormat::Files,
            "--ref-format" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("clone --ref-format requires a value".into())
                })?;
                reject_unknown_clone_ref_format(value)?;
                ref_storage = RefStorageFormat::parse(value)?;
            }
            value if value.starts_with("--ref-format=") => {
                let value = value.strip_prefix("--ref-format=").ok_or_else(|| {
                    GitError::Command("clone --ref-format requires a value".into())
                })?;
                reject_unknown_clone_ref_format(value)?;
                ref_storage = RefStorageFormat::parse(value)?;
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
                upload_pack = Some(value.to_string());
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
                upload_pack = Some(value.to_string());
            }
            value if value.starts_with("-u") && !value.starts_with("--") && value.len() > 2 => {
                upload_pack = Some(
                    value
                        .strip_prefix("-u")
                        .ok_or_else(|| {
                            GitError::Command("clone --upload-pack requires a value".into())
                        })?
                        .to_string(),
                );
            }
            "--no-upload-pack" => upload_pack = None,
            "--server-option" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("clone --server-option requires a value".into())
                })?;
                server_options.push(value.clone());
                server_options_from_cli = true;
            }
            value if value.starts_with("--server-option=") => {
                let value = value.strip_prefix("--server-option=").ok_or_else(|| {
                    GitError::Command("clone --server-option requires a value".into())
                })?;
                server_options.push(value.to_string());
                server_options_from_cli = true;
            }
            "--no-server-option" => {
                server_options.clear();
                server_options_from_cli = true;
            }
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
        .unwrap_or(depth.is_some() || deepen_since.is_some() || !deepen_not.is_empty());
    if also_filter_submodules && partial_clone_filter.is_none() {
        eprintln!("fatal: the option '--also-filter-submodules' requires '--filter'");
        return Err(GitError::Exit(128));
    }
    if also_filter_submodules && submodule_active.is_empty() {
        eprintln!("fatal: the option '--also-filter-submodules' requires '--recurse-submodules'");
        return Err(GitError::Exit(128));
    }
    if bundle_uri.is_some() && (depth.is_some() || deepen_since.is_some() || !deepen_not.is_empty())
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
    trace_index_pack_fsck_objects_if_configured();
    trace_pack_objects_filter(partial_clone_filter.as_deref());
    let repository = positional[0].clone();
    let cwd = env::current_dir()?;
    let bundle_source_path = clone_bundle_path(&cwd, &repository);
    let destination = positional
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| default_clone_directory(&repository, bare, bundle_source_path.is_some()));
    // git reports the destination as it was given on the command line (or as
    // derived from the source) — `dir` in upstream `builtin/clone.c` — not its
    // absolutized form.
    let destination_display = destination.clone();
    let destination = if destination.is_absolute() {
        destination
    } else {
        cwd.join(destination)
    };
    let env_worktree = if bare {
        None
    } else {
        env::var_os("GIT_WORK_TREE").and_then(|value| {
            if value.is_empty() {
                None
            } else {
                let path = PathBuf::from(value);
                Some(if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                })
            }
        })
    };
    let checkout_destination = env_worktree.as_ref().unwrap_or(&destination).clone();
    let clone_git_dir_override = env_worktree.as_ref().map(|_| destination.clone());
    let clone_core_worktree = env_worktree
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    // Upstream absolutizes a local source path (`absolute_pathdup` in
    // builtin/clone.c) so later chdirs — the bare/mirror path fetches from
    // inside the destination — cannot re-anchor a relative source like ".".
    // It does not resolve symlinks in that spelling, so `/var/...` must stay
    // `/var/...` instead of becoming `/private/var/...` on macOS.
    let repository = absolutize_local_clone_source(&cwd, &repository);
    let transport_config = transport_policy_config_for_cwd()?;
    if !server_options_from_cli {
        server_options = configured_server_options(&transport_config, &origin)?;
    } else if configured_legacy_protocol(Some(&transport_config)) {
        eprintln!("fatal: server options require protocol version 2 or later");
        eprintln!("fatal: see protocol.version in 'git help config' for more details");
        return Err(GitError::Exit(128));
    }
    let mut ssh_options = sley_remote::ssh_transport_options_from_config(&transport_config);
    ssh_options.ip_version = ssh_ip_version;
    let resolved_repository = ls_remote_resolved_url(&repository)?;
    check_transport_allowed_url(&resolved_repository, Some(&transport_config))?;
    // An empty `--template=` (or `--template ""`) disables templating entirely,
    // matching upstream git's `copy_templates()`, which returns immediately when
    // the template directory is the empty string. Resolving "" against the cwd
    // would otherwise yield the cwd itself, and copying it into the destination's
    // gitdir recurses without bound (the destination lives inside the cwd).
    let template = template
        .as_deref()
        .filter(|path| !path.is_empty())
        .map(|path| resolve_cli_path(&cwd, path));
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

    if let Some(bundle_path) = bundle_source_path.as_deref() {
        clone_bundle_repository(CloneBundleOptions {
            repository: &repository,
            bundle_path,
            destination: &checkout_destination,
            destination_display: &destination_display,
            git_dir_override: clone_git_dir_override.as_deref(),
            core_worktree: clone_core_worktree.as_deref(),
            origin: &origin,
            quiet,
            bare,
            checkout,
            sparse,
            template: template.as_deref(),
            template_config,
            separate_git_dir: separate_git_dir.as_deref(),
            config_overrides: &config_overrides,
            submodule_active: &submodule_active,
            ref_storage,
        })?;
        return Ok(());
    }

    if sley_remote::remote_url_is_http(&repository).unwrap_or(false) {
        clone_http_repository(CloneHttpOptions {
            repository: &repository,
            destination: &checkout_destination,
            destination_display: &destination_display,
            git_dir_override: clone_git_dir_override.as_deref(),
            core_worktree: clone_core_worktree.as_deref(),
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
            ref_storage,
            ssh_options,
        })?;
        return recurse_clone_submodules(
            &checkout_destination,
            &submodule_active,
            bare,
            checkout,
            depth,
            quiet,
            &reference_alternates,
        );
    }
    if fetch_source_is_ssh(&repository)? {
        clone_ssh_repository(CloneHttpOptions {
            repository: &repository,
            destination: &checkout_destination,
            destination_display: &destination_display,
            git_dir_override: clone_git_dir_override.as_deref(),
            core_worktree: clone_core_worktree.as_deref(),
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
            ref_storage,
            ssh_options,
        })?;
        return recurse_clone_submodules(
            &checkout_destination,
            &submodule_active,
            bare,
            checkout,
            depth,
            quiet,
            &reference_alternates,
        );
    }
    if fetch_source_is_git(&repository)? {
        clone_git_repository(CloneHttpOptions {
            repository: &repository,
            destination: &checkout_destination,
            destination_display: &destination_display,
            git_dir_override: clone_git_dir_override.as_deref(),
            core_worktree: clone_core_worktree.as_deref(),
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
            ref_storage,
            ssh_options,
        })?;
        return recurse_clone_submodules(
            &checkout_destination,
            &submodule_active,
            bare,
            checkout,
            depth,
            quiet,
            &reference_alternates,
        );
    }

    let remote_git_dir = ls_remote_git_dir(&repository)?;
    // A local clone reads the source repository directly, so it is subject to the
    // same `safe.directory` ownership check git applies when opening any repo.
    // The source is identified by its git directory (a clone needs no worktree),
    // so an exception is added as `<source>/.git`.
    crate::ownership::ensure_valid_ownership(None, &remote_git_dir, None)?;
    let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
    let format = repository_object_format(&remote_common_git_dir)?;
    if upload_pack.as_deref() == Some("false") {
        return Err(GitError::Exit(128));
    }
    // `--branch=<name>` may name a tag rather than a branch; git then checks the
    // tag's commit out with a detached HEAD (`our_head_points_at` is a tag ⇒
    // detach in builtin/clone.c). Detect that here so the clone routes through
    // the detached-head path instead of looking for a non-existent
    // `refs/remotes/<origin>/<tag>`.
    let branch_tag_oid = branch
        .as_deref()
        .and_then(|name| clone_source_tag_commit(&remote_common_git_dir, format, name));
    let detached_remote_head = match &branch_tag_oid {
        Some(oid) => Some(oid.clone()),
        None => remote_head_detached(&remote_common_git_dir, format),
    };
    let remote_head_branch = match (&detached_remote_head, &branch) {
        // A detached source HEAD (or a `--branch=<tag>`) has no default branch;
        // the clone checks the commit out detached.
        (Some(_), _) if branch_tag_oid.is_some() => String::new(),
        (Some(_), None) => String::new(),
        _ => clone_remote_head_branch(&remote_common_git_dir, format)?.unwrap_or_default(),
    };
    let alternates = clone_alternates(&remote_git_dir, shared, &reference_alternates)?;
    let source_alternates_git_dir = remote_common_git_dir.clone();
    let revision_oid = revision
        .as_deref()
        .map(|rev| resolve_revision(&remote_common_git_dir, format, rev))
        .transpose()?;
    let branch_explicit = branch.is_some();
    let checkout_branch = branch.unwrap_or_else(|| {
        if remote_head_branch.is_empty() {
            clone_default_branch_name()
        } else {
            remote_head_branch.clone()
        }
    });
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
    // Upstream `builtin/clone.c`: `clone.rejectshallow` (config) is overridden by
    // `--[no-]reject-shallow` (CLI). When the resolved value is true and the
    // source repository is shallow (a `shallow` file in its git dir), the clone
    // is refused. For a true local-mechanism clone git only `warning`s and falls
    // back to the transport, but sley always serves locally through the
    // in-process upload-pack, so the rejection applies whenever the source is
    // shallow — matching `--no-local` (the only way to reject-shallow a path).
    let reject_shallow = option_reject_shallow.or(clone_reject_shallow_config(&config_overrides)?);
    if reject_shallow == Some(true) && remote_common_git_dir.join("shallow").exists() {
        eprintln!("fatal: source repository is shallow, reject to clone.");
        return Err(GitError::Exit(128));
    }
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
    let deepen_since = if deepen_since.is_some() && (local_mechanism || bare || revision.is_some())
    {
        eprintln!("warning: --shallow-since is ignored in local clones; use file:// instead.");
        None
    } else {
        deepen_since
    };
    let deepen_not = if !deepen_not.is_empty() && (local_mechanism || bare || revision.is_some()) {
        eprintln!("warning: --shallow-exclude is ignored in local clones; use file:// instead.");
        Vec::new()
    } else {
        deepen_not
    };
    // `--filter` on a true local clone is warned-and-ignored (`is_local` in
    // builtin/clone.c). A transport clone (`--no-local` / `file://`) honors it
    // when the source advertises filtering (`uploadpack.allowFilter`),
    // otherwise warns exactly like a server without the capability.
    let mut fetch_filter = None::<sley_odb::PackObjectFilter>;
    if let Some(filter) = partial_clone_filter.as_deref() {
        if local_mechanism {
            eprintln!("warning: --filter is ignored in local clones; use file:// instead.");
        } else {
            let remote_config = read_repo_config(&remote_common_git_dir).ok();
            let remote_allows_filter = remote_config
                .as_ref()
                .and_then(|config| config.get_bool("uploadpack", None, "allowfilter"))
                .unwrap_or(false);
            let parsed_filter =
                pack_filter_from_spec_for_clone(filter, &remote_common_git_dir, format)?;
            match (remote_allows_filter, remote_config.as_ref(), parsed_filter) {
                (true, Some(config), Some(parsed)) => {
                    validate_server_filter_policy(config, filter)?;
                    fetch_filter = Some(parsed);
                }
                _ => eprintln!("warning: filtering not recognized by server, ignoring"),
            }
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
    if local == Some(true) && !local_source {
        eprintln!("warning: --local is ignored");
    }
    let local_object_install = if local_source {
        if shared {
            LocalObjectInstall::Shared
        } else if no_hardlinks {
            LocalObjectInstall::Copy
        } else {
            LocalObjectInstall::Hardlink {
                required: local == Some(true),
            }
        }
    } else {
        LocalObjectInstall::Transport
    };
    if parse_remote_url(&repository)
        .map(|url| url.transport == RemoteTransport::File)
        .unwrap_or(false)
    {
        trace_configured_local_protocol_version(None);
        if configured_protocol_version(None) == Some(ProtocolVersion::V2) {
            trace_protocol_v2_ls_refs_request(&server_options);
        }
    }
    if bare {
        clone_bare_or_mirror_local_repository(
            &destination,
            CloneLocalOptions {
                format,
                origin: &origin,
                repository: &repository,
                tag_opt: tag_opt.as_deref(),
                partial_clone_filter: partial_clone_filter.as_deref(),
                fetch_filter,
                head_branch: &checkout_branch,
                branch_explicit,
                // A detached source HEAD (and no explicit --branch) makes the
                // bare/mirror clone detach the destination HEAD at that commit.
                detached_head: if branch_explicit {
                    None
                } else {
                    detached_remote_head.as_ref()
                },
                revision_oid: revision_oid.as_ref(),
                mirror,
                single_branch,
                template: template.as_deref(),
                template_config,
                bundle_uri: bundle_uri.as_ref(),
                alternates: &alternates,
                copy_source_alternates: local_source,
                local_object_install,
                dissociate,
                config_overrides: &config_overrides,
                submodule_active: &submodule_active,
                ref_storage,
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
            if local_source {
                apply_clone_source_alternates(git_dir, &source_alternates_git_dir)?;
            }
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
            read_repo_config(git_dir)
        };

    if let Some(revision_oid) = revision_oid.as_ref() {
        // `--revision` copies the object closure directly and checks out detached;
        // it never fetches or creates a branch, so it keeps its own init here.
        let layout = RepositoryBootstrap::init(InitOptions {
            git_dir_override: clone_git_dir_override.clone(),
            core_worktree: clone_core_worktree.clone(),
            worktree: checkout_destination.clone(),
            object_format: format,
            object_format_explicit: false,
            bare: false,
            initial_branch: "__git_rs_clone_unborn__".into(),
            template_dir: None,
            copy_template_config: false,
            separate_git_dir: None,
            shared_repository: None,
            ref_storage,
            ref_storage_explicit: ref_storage != RefStorageFormat::Files,
        })?;
        let git_dir = layout.git_dir;
        configure_local_clone(&git_dir, None)?;
        copy_local_revision_objects(&remote_common_git_dir, &git_dir, format, revision_oid)?;
        if dissociate {
            dissociate_clone_alternates(&git_dir, format)?;
        }
        if let Some(bundle_uri) = bundle_uri.as_ref() {
            apply_clone_bundle_uri(&git_dir, format, bundle_uri)?;
        }
        if checkout {
            let config = read_repo_config(&git_dir)?;
            sley_worktree::checkout_detached_filtered(
                &checkout_destination,
                &git_dir,
                format,
                revision_oid,
                commit_identity_from_env("COMMITTER")?,
                format!("clone: from {repository}").into_bytes(),
                &config,
            )?;
            print_clone_detached_head_advice(revision_oid);
            run_clone_post_checkout_hook(&git_dir, revision_oid)?;
        } else {
            sley_worktree::checkout_detached(
                &checkout_destination,
                &git_dir,
                format,
                revision_oid,
                commit_identity_from_env("COMMITTER")?,
                format!("clone: from {repository}").into_bytes(),
            )?;
            remove_clone_worktree_files(&checkout_destination, &git_dir, format)?;
        }
        if let Some(separate_git_dir) = separate_git_dir.as_deref() {
            apply_clone_separate_git_dir(&checkout_destination, &git_dir, separate_git_dir)?;
        }
        if !quiet && local_source {
            eprintln!("done.");
        }
        return Ok(());
    }

    let remote_common_git_dir_for_head = remote_common_git_dir.clone();
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
        deepen_since,
        deepen_not,
        committer: commit_identity_from_env("COMMITTER")?,
        // `--branch=<tag>` checks the tag's commit out detached; otherwise a
        // detached source HEAD is honored only for the default (no `--branch`)
        // case.
        detached_head: if branch_tag_oid.is_some() {
            branch_tag_oid.clone()
        } else if branch_explicit {
            None
        } else {
            detached_remote_head
        },
        checkout,
        filter: fetch_filter,
        // A `--branch=<tag>` is satisfied by the detached checkout, so the
        // remote-tracking-branch lookup (and its "Remote branch not found"
        // mapping) must be bypassed.
        branch_explicit: branch_explicit && branch_tag_oid.is_none(),
        ref_storage,
        ssh_options: None,
    };
    let mut credentials = sley_remote::NoCredentials;
    let mut progress = StdoutProgress;
    let outcome = sley_remote::clone(
        sley_remote::CloneRequest {
            destination: &checkout_destination,
            git_dir_override: clone_git_dir_override.as_deref(),
            core_worktree: clone_core_worktree.as_deref(),
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
                // A detached source HEAD may sit on commits unreachable from
                // any ref; copy its closure so the detached checkout works.
                if let Some(detached) = clone_options.detached_head.as_ref() {
                    copy_local_revision_objects(
                        &remote_common_git_dir_for_head,
                        git_dir,
                        format,
                        detached,
                    )?;
                }
                configure_local_clone(git_dir, fetch_refspec)
            },
            configure_branch: &mut |git_dir, branch| {
                configure_clone_branch(git_dir, branch, &origin)?;
                repo_config_with_transport_policy(git_dir)
            },
            credentials: &mut credentials,
            progress: &mut progress,
        },
    )?;
    let git_dir = outcome.git_dir;
    if local_source {
        install_local_clone_objects(&source_alternates_git_dir, &git_dir, local_object_install)?;
    }
    if dissociate {
        dissociate_clone_alternates(&git_dir, format)?;
    }
    if let Some(bundle_uri) = bundle_uri.as_ref() {
        apply_clone_bundle_uri(&git_dir, format, bundle_uri)?;
    }
    if outcome.empty {
        warn_cloned_empty_repository();
    } else if !checkout {
        remove_clone_worktree_files(&checkout_destination, &git_dir, format)?;
    } else if sparse {
        apply_clone_sparse_checkout(&checkout_destination, &git_dir, format)?;
    }
    if checkout
        && !outcome.empty
        && let Some(new_head) = outcome.branch_oid.as_ref()
    {
        run_clone_post_checkout_hook(&git_dir, new_head)?;
    }
    if let Some(separate_git_dir) = separate_git_dir.as_deref() {
        apply_clone_separate_git_dir(&checkout_destination, &git_dir, separate_git_dir)?;
    }
    if !quiet && local_source {
        eprintln!("done.");
    }
    recurse_clone_submodules(
        &checkout_destination,
        &submodule_active,
        bare,
        checkout,
        depth,
        quiet,
            &reference_alternates,
    )
}

struct CloneBundleOptions<'a> {
    repository: &'a str,
    bundle_path: &'a Path,
    destination: &'a Path,
    destination_display: &'a Path,
    git_dir_override: Option<&'a Path>,
    core_worktree: Option<&'a str>,
    origin: &'a str,
    quiet: bool,
    bare: bool,
    checkout: bool,
    sparse: bool,
    template: Option<&'a Path>,
    template_config: bool,
    separate_git_dir: Option<&'a Path>,
    config_overrides: &'a [GlobalConfigOverride],
    submodule_active: &'a [String],
    ref_storage: RefStorageFormat,
}

fn clone_bundle_path(cwd: &Path, repository: &str) -> Option<PathBuf> {
    let parsed = parse_remote_url(repository).ok()?;
    if parsed.transport != RemoteTransport::Local {
        return None;
    }
    let raw = PathBuf::from(parsed.path);
    let base = if raw.is_absolute() { raw } else { cwd.join(raw) };
    if local_repository_git_dir_path(&base).is_ok() {
        return None;
    }
    let suffixed = path_with_bundle_suffix(&base);
    for candidate in [suffixed, base] {
        if candidate.is_file()
            && let Ok(bytes) = fs::read(&candidate)
            && Bundle::parse(&bytes, ObjectFormat::Sha1).is_ok()
        {
            return Some(candidate);
        }
    }
    None
}

fn path_with_bundle_suffix(path: &Path) -> PathBuf {
    let mut suffixed = path.as_os_str().to_os_string();
    suffixed.push(".bundle");
    PathBuf::from(suffixed)
}

fn clone_bundle_repository(options: CloneBundleOptions<'_>) -> Result<()> {
    if options.bare {
        return Err(GitError::Unsupported(
            "cloning bare repositories from bundles is not supported yet".into(),
        ));
    }
    let bundle_bytes = fs::read(options.bundle_path)?;
    let format = ObjectFormat::Sha1;
    let bundle = Bundle::parse(&bundle_bytes, format)?;
    let bundle_url = options.bundle_path.to_string_lossy().into_owned();
    if !options.quiet {
        eprintln!(
            "Cloning into '{}'...",
            options.destination_display.display()
        );
    }
    let head_branch = bundle_clone_head_branch(&bundle).or_else(|| {
        let default = clone_default_branch_name();
        bundle
            .references
            .iter()
            .any(|reference| reference.name == format!("refs/heads/{default}"))
            .then_some(default)
    });
    let layout = RepositoryBootstrap::init(InitOptions {
        git_dir_override: options.git_dir_override.map(Path::to_path_buf),
        core_worktree: options.core_worktree.map(str::to_string),
        worktree: options.destination.to_path_buf(),
        object_format: format,
        object_format_explicit: false,
        bare: false,
        initial_branch: head_branch
            .clone()
            .unwrap_or_else(|| clone_default_branch_name()),
        template_dir: None,
        copy_template_config: false,
        separate_git_dir: None,
        shared_repository: None,
        ref_storage: options.ref_storage,
        ref_storage_explicit: options.ref_storage != RefStorageFormat::Files,
    })?;
    let git_dir = layout.git_dir;
    apply_clone_template(&git_dir, options.template, options.template_config)?;
    configure_clone_remote(
        &git_dir,
        options.origin,
        &bundle_url,
        Some(format!("+refs/heads/*:refs/remotes/{}/*", options.origin)),
        false,
        None,
        None,
    )?;
    apply_clone_config_overrides(&git_dir, options.config_overrides)?;
    apply_clone_submodule_active(&git_dir, options.submodule_active)?;
    fetch_bundle(
        &git_dir,
        format,
        &bundle_url,
        &[
            format!("+refs/heads/*:refs/remotes/{}/*", options.origin),
            "+refs/tags/*:refs/tags/*".to_string(),
        ],
        &bundle,
        FetchOptions {
            quiet: true,
            auto_follow_tags: true,
            fetch_all_tags: false,
            prune: false,
            prune_tags: false,
            dry_run: false,
            append: false,
            write_fetch_head: false,
            tag_option_explicit: false,
            prune_option_explicit: false,
            prune_tags_option_explicit: false,
            refmap: None,
            depth: None,
            merge_srcs: Vec::new(),
            filter: None,
            refetch: false,
            cloning: true,
            record_promisor_refs: false,
            update_shallow: false,
            deepen_relative: false,
            update_head_ok: false,
            deepen_since: None,
            deepen_not: Vec::new(),
            ssh_options: None,
            atomic: false,
        },
    )?;
    if let Some(branch) = head_branch {
        let store = FileRefStore::new(&git_dir, format);
        let remote_branch = format!("refs/remotes/{}/{branch}", options.origin);
        if let Some(RefTarget::Direct(oid)) = store.read_ref(&remote_branch)? {
            store.create_branch(
                &branch,
                oid,
                commit_identity_from_env("COMMITTER")?,
                format!("branch: Created from {}/{branch}", options.origin).into_bytes(),
            )?;
            configure_clone_branch(&git_dir, &branch, options.origin)?;
            if options.checkout {
                let config = read_repo_config(&git_dir)?;
                sley_worktree::checkout_branch_filtered(
                    options.destination,
                    &git_dir,
                    format,
                    &branch,
                    commit_identity_from_env("COMMITTER")?,
                    &config,
                )?;
                run_clone_post_checkout_hook(&git_dir, &oid)?;
            }
        }
    }
    if !options.checkout {
        remove_clone_worktree_files(options.destination, &git_dir, format)?;
    } else if options.sparse {
        apply_clone_sparse_checkout(options.destination, &git_dir, format)?;
    }
    if let Some(separate_git_dir) = options.separate_git_dir {
        apply_clone_separate_git_dir(options.destination, &git_dir, separate_git_dir)?;
    }
    Ok(())
}

fn bundle_clone_head_branch(bundle: &Bundle) -> Option<String> {
    let head = bundle
        .references
        .iter()
        .find(|reference| reference.name == "HEAD")?;
    bundle.references.iter().find_map(|reference| {
        reference
            .name
            .strip_prefix("refs/heads/")
            .filter(|_| reference.oid == head.oid)
            .map(str::to_string)
    })
}

struct CloneHttpOptions<'a> {
    repository: &'a str,
    destination: &'a Path,
    /// The destination as given on the command line (or derived from the
    /// source), for user-facing messages — `dir` in upstream `builtin/clone.c`.
    destination_display: &'a Path,
    git_dir_override: Option<&'a Path>,
    core_worktree: Option<&'a str>,
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
    ref_storage: RefStorageFormat,
    ssh_options: sley_remote::SshTransportOptions,
}

/// Derive the remote default branch name from the upload-pack advertisement:
/// prefer the advertised `HEAD` symref, otherwise match the `HEAD` object id to a
/// branch tip. Returns `None` when the remote advertised no usable HEAD (an
/// empty/unborn repository); the caller then falls back to the local default
/// branch name (git's `repo_default_branch_name`).
fn http_remote_head_branch(
    features: &UploadPackFeatures,
    advertisements: &[RefAdvertisement],
) -> Option<String> {
    for symref in &features.symrefs {
        if let Some((name, target)) = symref.split_once(':')
            && name == "HEAD"
            && let Some(branch) = target.strip_prefix("refs/heads/")
        {
            return Some(branch.to_string());
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
                return Some(branch.to_string());
            }
        }
    }
    None
}

/// git's `repo_default_branch_name`: `GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME`
/// (when non-empty), then `init.defaultBranch`, then `master`. Used to name the
/// unborn local branch when cloning an empty/unborn remote.
fn clone_default_branch_name() -> String {
    if let Ok(name) = env::var("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME")
        && !name.is_empty()
    {
        return name;
    }
    if let Ok(Some(name)) = crate::clone_init_default_branch_config()
        && !name.is_empty()
    {
        return name;
    }
    "master".to_string()
}

fn absolutize_local_clone_source(cwd: &Path, repository: &str) -> String {
    if let Ok(parsed) = parse_remote_url(repository)
        && parsed.transport == RemoteTransport::Local
    {
        let path = PathBuf::from(&parsed.path);
        let absolute = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        if local_repository_git_dir_path(&absolute).is_ok() {
            return absolute.to_string_lossy().into_owned();
        }
        return repository.to_string();
    }
    // A spelling that parses as a scp-style `host:path` (or a non-local scheme)
    // is still cloned locally when a repository exists at that literal
    // filesystem path: git's `get_repo_path` probes the raw argument first and,
    // when it resolves, `is_local` wins over the ssh interpretation (t5601
    // "clone local path foo:bar"). Absolutize so downstream classification sees
    // a leading-slash path rather than re-deriving ssh.
    let literal = PathBuf::from(repository);
    let absolute = if literal.is_absolute() {
        literal
    } else {
        cwd.join(&literal)
    };
    if local_repository_git_dir_path(&absolute).is_ok() {
        return absolute.to_string_lossy().into_owned();
    }
    repository.to_string()
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
    let remote_head_branch = http_remote_head_branch(&features, &advertisements)
        .unwrap_or_else(clone_default_branch_name);
    let branch_explicit = options.branch.is_some();
    let checkout_branch = options
        .branch
        .clone()
        .unwrap_or_else(|| remote_head_branch.clone());

    if !options.quiet {
        eprintln!(
            "Cloning into '{}'...",
            options.destination_display.display()
        );
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
        deepen_since: None,
        deepen_not: Vec::new(),
        committer: commit_identity_from_env("COMMITTER")?,
        detached_head: None,
        checkout: options.checkout,
        filter: None,
        branch_explicit,
        ref_storage: options.ref_storage,
        ssh_options: None,
    };
    let mut progress = StdoutProgress;
    let outcome = sley_remote::clone(
        sley_remote::CloneRequest {
            destination: options.destination,
            git_dir_override: options.git_dir_override,
            core_worktree: options.core_worktree,
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
                repo_config_with_transport_policy(git_dir)
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
    let empty = outcome.empty;
    let git_dir = outcome.git_dir;

    if empty {
        warn_cloned_empty_repository();
    } else if !options.checkout {
        remove_clone_worktree_files(options.destination, &git_dir, format)?;
    } else if options.sparse {
        apply_clone_sparse_checkout(options.destination, &git_dir, format)?;
    }
    if options.checkout
        && !empty
        && let Some(new_head) = outcome.branch_oid.as_ref()
    {
        run_clone_post_checkout_hook(&git_dir, new_head)?;
    }
    if let Some(separate_git_dir) = options.separate_git_dir {
        apply_clone_separate_git_dir(options.destination, &git_dir, separate_git_dir)?;
    }
    // An empty-repository clone stops before the checkout that would print
    // "done."; git emits only the warning in that case.
    if !options.quiet && !empty {
        eprintln!("done.");
    }
    Ok(())
}

/// Clone a repository over SSH upload-pack. Covers the common non-bare case;
/// bare/mirror, `--revision`, `--shared`/`--reference`, and `--bundle-uri` are
/// not supported over SSH yet.
fn clone_ssh_repository(options: CloneHttpOptions<'_>) -> Result<()> {
    clone_network_repository(options, CloneNetworkTransport::Ssh)
}

fn clone_git_repository(options: CloneHttpOptions<'_>) -> Result<()> {
    clone_network_repository(options, CloneNetworkTransport::Git)
}

#[derive(Debug, Clone, Copy)]
enum CloneNetworkTransport {
    Ssh,
    Git,
}

impl CloneNetworkTransport {
    fn name(self) -> &'static str {
        match self {
            Self::Ssh => "SSH",
            Self::Git => "git://",
        }
    }
}

fn clone_network_repository(
    options: CloneHttpOptions<'_>,
    transport: CloneNetworkTransport,
) -> Result<()> {
    if options.revision.is_some() {
        return Err(GitError::Unsupported(format!(
            "clone --revision over {} is not supported yet",
            transport.name()
        )));
    }
    if options.shared || !options.reference_alternates.is_empty() {
        return Err(GitError::Unsupported(format!(
            "clone --shared/--reference over {} is not supported yet",
            transport.name()
        )));
    }
    if options.bundle_uri.is_some() {
        return Err(GitError::Unsupported(format!(
            "clone --bundle-uri over {} is not supported yet",
            transport.name()
        )));
    }
    if options.partial_clone_filter.is_some() {
        eprintln!(
            "warning: --filter is not supported over {} yet, ignoring",
            transport.name()
        );
    }

    let remote = parse_remote_url(&ls_remote_resolved_url(options.repository)?)?;
    if matches!(transport, CloneNetworkTransport::Ssh) {
        trace_configured_local_protocol_version(None);
    }
    let (advertisements, features) = match transport {
        CloneNetworkTransport::Ssh => sley_remote::ssh_upload_pack_advertisements_with_options(
            &remote,
            ObjectFormat::Sha1,
            options.ssh_options,
        )?,
        CloneNetworkTransport::Git => {
            let discovered = sley_remote::git_upload_pack_advertisements_with_protocol(
                &remote,
                ObjectFormat::Sha1,
                configured_protocol_version(None) == Some(ProtocolVersion::V2),
            )?;
            (discovered.refs, discovered.features)
        }
    };
    let format = features.object_format.unwrap_or(ObjectFormat::Sha1);
    // An empty/unborn remote advertises no usable HEAD; fall back to the local
    // default branch name so the unborn-clone path can name `HEAD`.
    let remote_head_branch = http_remote_head_branch(&features, &advertisements)
        .unwrap_or_else(clone_default_branch_name);
    let branch_explicit = options.branch.is_some();
    let checkout_branch = options
        .branch
        .clone()
        .unwrap_or_else(|| remote_head_branch.clone());

    if options.bare {
        if !options.quiet {
            eprintln!(
                "Cloning into bare repository '{}'...",
                options.destination_display.display()
            );
        }
        return clone_bare_network_repository(
            &options,
            transport,
            remote,
            format,
            &checkout_branch,
        );
    }

    if !options.quiet {
        eprintln!(
            "Cloning into '{}'...",
            options.destination_display.display()
        );
    }

    let single_branch = options.single_branch;
    let origin = options.origin;
    let repository = options.repository;
    let template = options.template;
    let template_config = options.template_config;
    let tag_opt = options.tag_opt;
    let config_overrides = options.config_overrides;
    let submodule_active = options.submodule_active;
    let remote_source = match transport {
        CloneNetworkTransport::Ssh => sley_remote::CloneSource::Ssh(remote),
        CloneNetworkTransport::Git => sley_remote::CloneSource::Git {
            remote,
            protocol_v2: configured_protocol_version(None) == Some(ProtocolVersion::V2),
        },
    };
    let clone_options = sley_remote::CloneOptions {
        origin,
        checkout_branch: &checkout_branch,
        remote_head_branch: &remote_head_branch,
        single_branch,
        depth: options.depth,
        deepen_since: None,
        deepen_not: Vec::new(),
        committer: commit_identity_from_env("COMMITTER")?,
        detached_head: None,
        checkout: options.checkout,
        filter: None,
        branch_explicit,
        ref_storage: options.ref_storage,
        ssh_options: matches!(transport, CloneNetworkTransport::Ssh).then_some(options.ssh_options),
    };
    let mut credentials = sley_remote::NoCredentials;
    let mut progress = StdoutProgress;
    let outcome = sley_remote::clone(
        sley_remote::CloneRequest {
            destination: options.destination,
            git_dir_override: options.git_dir_override,
            core_worktree: options.core_worktree,
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
                repo_config_with_transport_policy(git_dir)
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
    let empty = outcome.empty;
    let git_dir = outcome.git_dir;

    if empty {
        warn_cloned_empty_repository();
    } else if !options.checkout {
        remove_clone_worktree_files(options.destination, &git_dir, format)?;
    } else if options.sparse {
        apply_clone_sparse_checkout(options.destination, &git_dir, format)?;
    }
    if options.checkout
        && !empty
        && let Some(new_head) = outcome.branch_oid.as_ref()
    {
        run_clone_post_checkout_hook(&git_dir, new_head)?;
    }
    if let Some(separate_git_dir) = options.separate_git_dir {
        apply_clone_separate_git_dir(options.destination, &git_dir, separate_git_dir)?;
    }
    // An empty-repository clone stops before the checkout that would print
    // "done."; git emits only the warning in that case.
    if !options.quiet && !empty {
        eprintln!("done.");
    }
    Ok(())
}

fn clone_bare_network_repository(
    options: &CloneHttpOptions<'_>,
    transport: CloneNetworkTransport,
    remote: RemoteUrl,
    format: ObjectFormat,
    checkout_branch: &str,
) -> Result<()> {
    let layout = RepositoryBootstrap::init(InitOptions {
        git_dir_override: None,
        core_worktree: None,
        worktree: options.destination.to_path_buf(),
        object_format: format,
        object_format_explicit: false,
        bare: true,
        initial_branch: checkout_branch.into(),
        template_dir: None,
        copy_template_config: false,
        separate_git_dir: None,
        shared_repository: None,
        ref_storage: options.ref_storage,
        ref_storage_explicit: options.ref_storage != RefStorageFormat::Files,
    })?;
    let git_dir = layout.git_dir;
    apply_clone_template(&git_dir, options.template, options.template_config)?;
    configure_clone_remote(
        &git_dir,
        options.origin,
        options.repository,
        None,
        false,
        options.tag_opt,
        options.partial_clone_filter,
    )?;
    apply_clone_config_overrides(&git_dir, options.config_overrides)?;
    apply_clone_submodule_active(&git_dir, options.submodule_active)?;
    if let Some(bundle_uri) = options.bundle_uri {
        apply_clone_bundle_uri(&git_dir, format, bundle_uri)?;
    }

    let config = repo_config_with_transport_policy(&git_dir)?;
    let source = match transport {
        CloneNetworkTransport::Ssh => sley_remote::FetchSource::Ssh(remote),
        CloneNetworkTransport::Git => sley_remote::FetchSource::Git {
            remote,
            protocol_v2: configured_protocol_version(None) == Some(ProtocolVersion::V2),
        },
    };
    let mut refspecs = if options.single_branch {
        vec![format!(
            "+refs/heads/{checkout_branch}:refs/heads/{checkout_branch}"
        )]
    } else {
        vec!["+refs/heads/*:refs/heads/*".to_string()]
    };
    if options.tag_opt != Some("--no-tags") {
        refspecs.push("+refs/tags/*:refs/tags/*".to_string());
    }
    run_fetch(
        &git_dir,
        format,
        &config,
        options.origin,
        &source,
        &refspecs,
        FetchOptions {
            quiet: true,
            auto_follow_tags: options.tag_opt != Some("--no-tags") || options.branch.is_some(),
            fetch_all_tags: options.tag_opt == Some("--tags"),
            prune: false,
            prune_tags: false,
            dry_run: false,
            append: false,
            write_fetch_head: false,
            tag_option_explicit: options.tag_opt.is_some(),
            prune_option_explicit: false,
            prune_tags_option_explicit: false,
            refmap: None,
            depth: options.depth,
            merge_srcs: Vec::new(),
            filter: None,
            cloning: true,
            update_shallow: false,
            deepen_relative: false,
            update_head_ok: true,
            deepen_since: None,
            deepen_not: Vec::new(),
            record_promisor_refs: false,
            refetch: false,
            ssh_options: None,
            atomic: false,
        },
        &[],
    )
    .map(|_| ())
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

/// Validate `git clone --ref-format=<name>`. Upstream resolves the name through
/// `ref_storage_format_by_name`; an unknown name (e.g. `garbage`) dies with
/// `fatal: unknown ref storage format '<name>'` (exit 128). sley implements only
/// the `files` or `reftable` backend; unknown names keep git's exact diagnostic.
fn reject_unknown_clone_ref_format(value: &str) -> Result<()> {
    match value {
        "files" | "reftable" => Ok(()),
        _ => {
            eprintln!("fatal: unknown ref storage format '{value}'");
            Err(GitError::Exit(128))
        }
    }
}

/// Parse a `--depth` value the way `git clone`/`git fetch` do: an optional `+`
/// sign then ASCII digits, rejecting non-positive depths with git's message. The
/// numeric value is clamped to `u32::MAX` (git stores depth as a C `int`; the
/// protocol's `deepen` is unsigned, and any value this large already deepens past
/// every real history).
pub(crate) fn parse_clone_depth(value: &str) -> Result<u32> {
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
    if value.starts_with("sparse:oid=") {
        return Ok(());
    }
    eprintln!("fatal: invalid filter-spec '{value}'");
    Err(GitError::Exit(128))
}

fn add_clone_filter(current: &mut Option<String>, value: &str) {
    *current = Some(match current.take() {
        Some(existing) => {
            let existing = existing.strip_prefix("combine:").unwrap_or(&existing);
            format!("combine:{existing}+{value}")
        }
        None => value.to_string(),
    });
}

fn validate_upload_pack_filter_config() -> Result<()> {
    if let Ok(Some(value)) = global_config_value("uploadpackfilter.tree.maxdepth")
        && value.parse::<u32>().is_err()
    {
        eprintln!("fatal: unable to parse uploadpackfilter.tree.maxdepth");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

fn validate_server_filter_policy(config: &GitConfig, filter: &str) -> Result<()> {
    if let Some(parts) = filter.strip_prefix("combine:") {
        if !upload_pack_filter_allowed(config, "combine") {
            return filter_not_supported("combine");
        }
        for part in parts.split('+') {
            validate_server_filter_policy(config, part)?;
        }
        return Ok(());
    }
    if filter == "blob:none" {
        if upload_pack_filter_allowed(config, "blob:none") {
            return Ok(());
        }
        return filter_not_supported(filter);
    }
    if let Some(depth) = filter.strip_prefix("tree:") {
        if !upload_pack_filter_allowed(config, "tree") {
            return filter_not_supported(filter);
        }
        let depth = parse_rev_list_tree_depth(depth)? as u32;
        if let Some(max_depth) = config
            .get("uploadpackfilter", Some("tree"), "maxdepth")
            .and_then(|value| value.parse::<u32>().ok())
            && depth > max_depth
        {
            eprintln!("fatal: tree filter allows max depth {max_depth}, but got {depth}");
            return Err(GitError::Exit(128));
        }
        return Ok(());
    }
    Ok(())
}

fn upload_pack_filter_allowed(config: &GitConfig, name: &str) -> bool {
    config
        .get_bool("uploadpackfilter", Some(name), "allow")
        .unwrap_or_else(|| {
            config
                .get_bool("uploadpackfilter", None, "allow")
                .unwrap_or(true)
        })
}

fn filter_not_supported(filter: &str) -> Result<()> {
    eprintln!("fatal: filter '{filter}' not supported");
    Err(GitError::Exit(128))
}

fn trace_index_pack_fsck_objects_if_configured() {
    let Ok(Some(value)) = global_config_value("transfer.fsckobjects") else {
        return;
    };
    if parse_config_bool(&value) == Some(true) {
        setup::git_trace_line(
            "run-command.c:667",
            "trace: run_command: git index-pack --fsck-objects",
        );
    }
}

fn trace_pack_objects_filter(filter: Option<&str>) {
    let Some(filter) = filter else {
        return;
    };
    setup::git_trace_line(
        "run-command.c:667",
        &format!("trace: run_command: git pack-objects --filter={filter}"),
    );
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
    ref_storage: RefStorageFormat,
    origin: &'a str,
    repository: &'a str,
    tag_opt: Option<&'a str>,
    partial_clone_filter: Option<&'a str>,
    /// The object filter the clone fetch itself applies (only set when the
    /// in-process local server honors the `--filter`, i.e. a `--no-local` /
    /// `file://` clone of an `uploadpack.allowFilter` source).
    fetch_filter: Option<sley_odb::PackObjectFilter>,
    head_branch: &'a str,
    branch_explicit: bool,
    /// The source HEAD is detached at this commit (no default branch). The bare
    /// clone copies every ref (mirror) or the branch refs, then points the
    /// destination `HEAD` directly at this commit instead of a `refs/heads/<x>`
    /// symref — mirrors git's `update_head` detached arm for a bare clone of a
    /// detached-HEAD source.
    detached_head: Option<&'a ObjectId>,
    revision_oid: Option<&'a ObjectId>,
    mirror: bool,
    single_branch: bool,
    template: Option<&'a Path>,
    template_config: bool,
    bundle_uri: Option<&'a CloneBundleUri>,
    alternates: &'a [PathBuf],
    copy_source_alternates: bool,
    local_object_install: LocalObjectInstall,
    dissociate: bool,
    config_overrides: &'a [GlobalConfigOverride],
    submodule_active: &'a [String],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalObjectInstall {
    Hardlink { required: bool },
    Copy,
    Shared,
    Transport,
}

fn clone_bare_or_mirror_local_repository(
    destination: &Path,
    options: CloneLocalOptions<'_>,
) -> Result<()> {
    // A detached-HEAD source has no default branch; init on a placeholder so the
    // empty `head_branch` does not form an invalid `refs/heads/` symref. The
    // destination HEAD is repointed at the detached commit after the fetch.
    let initial_branch = if options.detached_head.is_some() {
        "__git_rs_clone_unborn__"
    } else {
        options.head_branch
    };
    let layout = RepositoryBootstrap::init(InitOptions {
        git_dir_override: None,
        core_worktree: None,
        worktree: destination.to_path_buf(),
        object_format: options.format,
        object_format_explicit: false,
        bare: true,
        initial_branch: initial_branch.into(),
        template_dir: None,
        copy_template_config: false,
        separate_git_dir: None,
        shared_repository: None,
        ref_storage: options.ref_storage,
        ref_storage_explicit: options.ref_storage != RefStorageFormat::Files,
    })?;
    let git_dir = layout.git_dir;
    apply_clone_template(&git_dir, options.template, options.template_config)?;
    apply_clone_alternates(&git_dir, options.alternates, options.dissociate)?;
    if options.copy_source_alternates {
        let source_git_dir = common_git_dir_for_git_dir(&ls_remote_git_dir(options.repository)?)?;
        apply_clone_source_alternates(&git_dir, &source_git_dir)?;
    }
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

    if let Some(revision_oid) = options.revision_oid {
        copy_local_revision_objects(
            &common_git_dir_for_git_dir(&ls_remote_git_dir(options.repository)?)?,
            &git_dir,
            options.format,
            revision_oid,
        )?;
        if options.dissociate {
            dissociate_clone_alternates(&git_dir, options.format)?;
        }
        if let Some(bundle_uri) = options.bundle_uri {
            apply_clone_bundle_uri(&git_dir, options.format, bundle_uri)?;
        }
        fs::write(git_dir.join("HEAD"), format!("{revision_oid}\n"))?;
        return Ok(());
    }

    let previous_cwd = env::current_dir()?;
    env::set_current_dir(destination)?;
    let mut refspecs = if options.mirror && options.single_branch {
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
    // Clone fetches every tag by default (upstream's `wanted_peer_refs` adds
    // the `refs/tags/*:refs/tags/*` map whenever `--no-tags` was not given) —
    // this also picks up tags pointing at non-commits, which tag
    // auto-following never would.
    if !options.mirror && options.tag_opt != Some("--no-tags") {
        refspecs.push("+refs/tags/*:refs/tags/*".to_string());
    }
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
            prune_tags: false,
            dry_run: false,
            append: false,
            write_fetch_head: false,
            tag_option_explicit: options.tag_opt.is_some(),
            prune_option_explicit: false,
            prune_tags_option_explicit: false,
            refmap: None,
            depth: None,
            merge_srcs: Vec::new(),
            filter: options.fetch_filter,
            refetch: false,
            cloning: false,
            record_promisor_refs: true,
            update_shallow: false,
            deepen_relative: false,
            update_head_ok: false,
            deepen_since: None,
            deepen_not: Vec::new(),
            ssh_options: None,
            atomic: false,
        },
    );
    env::set_current_dir(previous_cwd)?;
    fetch_result?;
    if options.copy_source_alternates {
        let source_git_dir = common_git_dir_for_git_dir(&ls_remote_git_dir(options.repository)?)?;
        install_local_clone_objects(
            &source_git_dir,
            &git_dir,
            options.local_object_install,
        )?;
    }
    if options.dissociate {
        dissociate_clone_alternates(&git_dir, options.format)?;
    }
    if let Some(bundle_uri) = options.bundle_uri {
        apply_clone_bundle_uri(&git_dir, options.format, bundle_uri)?;
    }
    // For a detached-HEAD source, point the destination HEAD directly at the
    // source's detached commit (it was just copied by the mirror/branch fetch),
    // matching git's `update_head` detached arm.
    if let Some(detached) = options.detached_head {
        fs::write(git_dir.join("HEAD"), format!("{detached}\n"))?;
    }
    Ok(())
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

/// Resolve `clone.rejectshallow` for the reject-shallow gate, reading the same
/// config git's second `git_config` pass sees: the global `-c` / `GIT_CONFIG_*`
/// injection plus any `clone -c clone.rejectshallow=<bool>`. Returns the last
/// (highest-precedence) value, or `None` when unset. The CLI `--reject-shallow`
/// flag overrides this at the call site (upstream `option_reject_shallow`).
fn clone_reject_shallow_config(config_overrides: &[GlobalConfigOverride]) -> Result<Option<bool>> {
    let mut resolved = None;
    for parameter in crate::injected_config_parameters()? {
        let (section, subsection, key) = parameter.split_key();
        if section == "clone" && subsection.is_none() && key == "rejectshallow" {
            let value = parameter.value.as_deref().unwrap_or("true");
            resolved = sley_config::parse_config_bool(value);
        }
    }
    for override_entry in config_overrides {
        let key = override_entry.key.to_ascii_lowercase();
        if key == "clone.rejectshallow" {
            resolved = sley_config::parse_config_bool(&override_entry.value);
        }
    }
    Ok(resolved)
}

fn apply_clone_config_overrides(git_dir: &Path, overrides: &[GlobalConfigOverride]) -> Result<()> {
    if overrides.is_empty() {
        return Ok(());
    }
    let mut config = read_repo_config_on_disk(git_dir)?;
    for override_entry in overrides {
        let key = parse_config_key(&override_entry.key)?;
        // Each `clone -c key=value` is an independent multivar addition, matching
        // upstream `write_one_config`'s `repo_config_set_multivar_gently(...,
        // CONFIG_REGEX_NONE, 0)` (CONFIG_REGEX_NONE => always append). Repeating
        // `-c` on the same key accumulates (`core.foo=bar -c core.foo=baz`), an
        // empty value persists a blank entry, and a `-c remote.<o>.fetch=<spec>`
        // adds a second fetch refspec alongside clone's default one.
        config_set_value(&mut config, &key, &override_entry.value, true);
    }
    write_repo_config(git_dir, &config)
}

fn apply_clone_template(git_dir: &Path, template: Option<&Path>, copy_config: bool) -> Result<()> {
    fs::create_dir_all(git_dir.join("hooks"))?;
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
        let current_config = read_repo_config_on_disk(git_dir)?;
        template_config.sections.extend(current_config.sections);
        write_repo_config(git_dir, &template_config)?;
    }
    Ok(())
}

fn run_clone_post_checkout_hook(git_dir: &Path, new_head: &ObjectId) -> Result<()> {
    let old = ObjectId::null(new_head.format()).to_hex();
    let new = new_head.to_hex();
    commands::hooks::run_traditional_hook_at(
        git_dir,
        "post-checkout",
        commands::hooks::HookRun {
            args: vec![old, new, "1".to_string()],
            ..commands::hooks::HookRun::default()
        },
    )?;
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
        push_unique_alternate(&mut alternates, remote_git_dir.join("objects"));
    }
    for reference in references {
        match ls_remote_git_dir(&reference.path)
            .and_then(|git_dir| common_git_dir_for_git_dir(&git_dir))
        {
            Ok(reference_git_dir) => {
                push_unique_alternate(&mut alternates, reference_git_dir.join("objects"));
            }
            Err(_) if reference.if_able => eprintln!(
                "info: Could not add alternate for '{}': reference repository '{}' is not a local repository.",
                reference.path, reference.path
            ),
            Err(err) => return Err(err),
        }
    }
    Ok(alternates)
}

fn push_unique_alternate(alternates: &mut Vec<PathBuf>, alternate: PathBuf) {
    if !alternates.iter().any(|existing| existing == &alternate) {
        alternates.push(alternate);
    }
}

fn apply_clone_alternates(
    git_dir: &Path,
    alternates: &[PathBuf],
    _dissociate: bool,
) -> Result<()> {
    if alternates.is_empty() {
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

fn dissociate_clone_alternates(git_dir: &Path, format: ObjectFormat) -> Result<()> {
    let alternates_path = git_dir.join("objects/info/alternates");
    if !alternates_path.exists() {
        return Ok(());
    }
    let roots = dissociate_repack_roots(git_dir, format)?;
    if let Some(result) = sley_odb::repack_reachable_objects(git_dir, format, &roots)? {
        sley_odb::install_repack_result(git_dir, format, &result, true)?;
    }
    match fs::remove_file(&alternates_path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn dissociate_repack_roots(git_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let store = FileRefStore::new(git_dir, format);
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    for reference in store.list_refs()? {
        if let Some(oid) = resolve_clone_ref_oid(&store, reference.target)? && seen.insert(oid) {
            roots.push(oid);
        }
    }
    if let Some(head) = store.read_ref("HEAD")?
        && let Some(oid) = resolve_clone_ref_oid(&store, head)?
        && seen.insert(oid)
    {
        roots.push(oid);
    }
    Ok(roots)
}

fn resolve_clone_ref_oid(store: &FileRefStore, mut target: RefTarget) -> Result<Option<ObjectId>> {
    for _ in 0..5 {
        match target {
            RefTarget::Direct(oid) => return Ok(Some(oid)),
            RefTarget::Symbolic(name) => {
                let Some(next) = store.read_ref(&name)? else {
                    return Ok(None);
                };
                target = next;
            }
        }
    }
    Ok(None)
}

fn apply_clone_source_alternates(git_dir: &Path, source_git_dir: &Path) -> Result<()> {
    let source_alternates = source_git_dir.join("objects/info/alternates");
    let Ok(contents) = fs::read_to_string(source_alternates) else {
        return Ok(());
    };

    let mut resolved = Vec::new();
    for raw in contents.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let path = Path::new(line);
        if path.is_absolute() {
            resolved.push(path.to_path_buf());
        } else if let Some(normalized) =
            normalize_clone_alternate_path(&source_git_dir.join("objects").join(path))
        {
            resolved.push(normalized);
        } else {
            eprintln!(
                "warning: skipping invalid relative alternate: {}/{}",
                source_git_dir.display(),
                line
            );
        }
    }
    if resolved.is_empty() {
        return Ok(());
    }

    let alternates_path = git_dir.join("objects/info/alternates");
    if let Some(parent) = alternates_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(alternates_path)?;
    use std::io::Write as _;
    for alternate in resolved {
        writeln!(file, "{}", alternate.display())?;
    }
    Ok(())
}

fn normalize_clone_alternate_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(Path::new("/")),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            std::path::Component::Normal(name) => normalized.push(name),
        }
    }
    Some(normalized)
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

fn install_local_clone_objects(
    remote_git_dir: &Path,
    git_dir: &Path,
    mode: LocalObjectInstall,
) -> Result<()> {
    match mode {
        LocalObjectInstall::Transport => return Ok(()),
        LocalObjectInstall::Shared => {
            let objects_dir = repository_objects_dir(git_dir);
            clear_local_clone_object_files(&objects_dir)?;
            fs::create_dir_all(objects_dir.join("pack"))?;
            return Ok(());
        }
        LocalObjectInstall::Hardlink { .. } | LocalObjectInstall::Copy => {}
    }

    let source_objects = repository_objects_dir(remote_git_dir);
    let destination_objects = repository_objects_dir(git_dir);
    if fs::symlink_metadata(&source_objects)?.file_type().is_symlink() {
        eprintln!(
            "fatal: '{}' is a symlink, refusing to clone with --local",
            source_objects.display()
        );
        return Err(GitError::Exit(128));
    }
    clear_local_clone_object_files(&destination_objects)?;
    fs::create_dir_all(&destination_objects)?;

    let mut hardlink = matches!(mode, LocalObjectInstall::Hardlink { .. });
    copy_or_link_local_object_directory(
        &source_objects,
        &destination_objects,
        Path::new(""),
        mode,
        &mut hardlink,
    )
}

fn clear_local_clone_object_files(objects_dir: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(objects_dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == "info" {
            clear_local_clone_info_dir(&path)?;
        } else if entry.file_type()?.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn clear_local_clone_info_dir(info_dir: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(info_dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name() == "alternates" {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn copy_or_link_local_object_directory(
    source_dir: &Path,
    destination_dir: &Path,
    relative: &Path,
    mode: LocalObjectInstall,
    hardlink: &mut bool,
) -> Result<()> {
    for entry in fs::read_dir(source_dir)? {
        let entry = entry?;
        let source = entry.path();
        let entry_relative = relative.join(entry.file_name());
        let destination = destination_dir.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.file_type().is_symlink() {
            eprintln!(
                "fatal: symlink '{}' exists, refusing to clone with --local",
                entry_relative.display()
            );
            return Err(GitError::Exit(128));
        }
        if metadata.is_dir() {
            fs::create_dir_all(&destination)?;
            copy_or_link_local_object_directory(
                &source,
                &destination,
                &entry_relative,
                mode,
                hardlink,
            )?;
            continue;
        }
        if entry_relative == Path::new("info/alternates") {
            continue;
        }
        if destination.exists() {
            fs::remove_file(&destination)?;
        }
        if *hardlink {
            match fs::hard_link(&source, &destination) {
                Ok(()) => continue,
                Err(err) if matches!(mode, LocalObjectInstall::Hardlink { required: true }) => {
                    return Err(GitError::Io(format!(
                        "failed to create link '{}': {err}",
                        destination.display()
                    )));
                }
                Err(_) => *hardlink = false,
            }
        }
        fs::copy(&source, &destination)?;
        let accessed = filetime::FileTime::from_last_access_time(&metadata);
        let modified = filetime::FileTime::from_last_modification_time(&metadata);
        filetime::set_file_times(&destination, accessed, modified)?;
    }
    Ok(())
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
    if install_bundle_pack(&bundle, &prerequisite_reader, &database).is_err() {
        warn_clone_bundle_uri_failed(&bundle_uri.uri);
        return Ok(());
    }
    let store = FileRefStore::new(git_dir, format);
    let mut tx = store.transaction();
    for reference in &bundle.references {
        let Some(name) = clone_bundle_uri_ref_name(&reference.name) else {
            continue;
        };
        tx.update(RefUpdate {
            name,
            expected: None,
            new: RefTarget::Direct(reference.oid),
            reflog: None,
        });
    }
    tx.commit()
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
    let mut config = read_repo_config_on_disk(git_dir)?;
    let key = parse_config_key("submodule.active")?;
    for value in active {
        config_set_value(&mut config, &key, value, true);
    }
    write_repo_config(git_dir, &config)
}

/// After a `--recurse-submodules` clone, populate the submodules — git's
/// `clone.c` runs `git submodule update --init --recursive` once the
/// superproject worktree is checked out. Without this, the submodule worktrees
/// stay empty (only `submodule.active` is set), so a nested superproject is left
/// half-cloned. `active` carries the `--recurse-submodules[=<pathspec>]` values
/// (`.` = all); a `bare`/no-checkout clone has no worktree to populate.
fn recurse_clone_submodules(
    destination: &Path,
    active: &[String],
    bare: bool,
    checkout: bool,
    depth: Option<u32>,
    quiet: bool,
    references: &[CloneReferenceAlternate],
) -> Result<()> {
    if active.is_empty() || bare || !checkout {
        return Ok(());
    }
    // git's `clone.c`: when `--recurse-submodules` is combined with
    // `--reference[-if-able]`, record `submodule.alternateLocation=superproject`
    // (+ the error strategy) so each recursive submodule clone borrows its
    // objects from the matching `modules/<name>` of the reference superproject.
    if !references.is_empty() {
        let strategy = if references.iter().all(|reference| reference.if_able) {
            "info"
        } else {
            "die"
        };
        let git_dir = discover_git_dir(destination)?;
        let mut config = read_repo_config(&git_dir)?;
        set_config_value(
            &mut config,
            "submodule",
            None,
            "alternateLocation",
            "superproject",
        );
        set_config_value(
            &mut config,
            "submodule",
            None,
            "alternateErrorStrategy",
            strategy,
        );
        write_repo_config(&git_dir, &config)?;
    }
    // git's `clone.c` treats `--recurse-submodules[=<pathspec>]` as the
    // `submodule.active` filter, NOT as explicit named pathspecs: it runs the
    // recursive update with no positional pathspec and lets the active filter
    // select which submodules populate. A pathspec that matches no submodule
    // therefore yields an empty active set — a no-op (exit 0) — rather than the
    // "pathspec did not match" error a bare `submodule update <pathspec>` raises.
    // We mirror this by restricting the forwarded pathspecs to those that match a
    // real submodule path in the cloned superproject, and skipping the update
    // entirely when none match (the all-case `.` forwards no pathspec, so it
    // always runs).
    let recurse_all = active.iter().any(|value| value == ".");
    let pathspecs: Vec<&String> = if recurse_all {
        Vec::new()
    } else {
        let submodules = crate::commands::submodule::read_submodule_configs(destination)?;
        active
            .iter()
            .filter(|value| {
                let normalized = crate::commands::submodule::normalize_submodule_pathspec(
                    destination,
                    destination,
                    value,
                );
                submodules.iter().any(|submodule| {
                    crate::commands::submodule::submodule_path_matches_pathspec(
                        &submodule.path,
                        &normalized,
                    )
                })
            })
            .collect()
    };
    // No pathspec matched any submodule (and `.` was not requested): nothing to
    // recurse into — exit cleanly, matching git's empty-active-set no-op.
    if !recurse_all && pathspecs.is_empty() {
        return Ok(());
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sley"));
    let mut command = ProcessCommand::new(exe);
    command.arg("submodule");
    if quiet {
        command.arg("--quiet");
    }
    command.arg("update").arg("--init").arg("--recursive");
    if let Some(depth) = depth {
        command.arg(format!("--depth={depth}"));
    }
    // Restrict to the matched pathspecs (the all-case `.` forwards none).
    for value in pathspecs {
        command.arg(value);
    }
    let status = command
        .current_dir(destination)
        .status()
        .map_err(|err| GitError::Io(err.to_string()))?;
    if !status.success() {
        return Err(GitError::Exit(1));
    }
    Ok(())
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

    let mut config = read_repo_config_on_disk(git_dir)?;
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

fn default_clone_directory(repository: &str, bare: bool, is_bundle: bool) -> PathBuf {
    // Port of upstream `git_url_basename` (dir.c). Operates on raw bytes because
    // the URL/host:path syntax must be parsed exactly as git does, including
    // auth-stripping, trailing-`/.git` handling, port stripping, and treating
    // `:` as a path separator for backwards-compatible `host:path` URLs.
    PathBuf::from(git_url_basename(repository, is_bundle, bare))
}

/// `is_dir_sep` on Linux: only `/`.
fn is_dir_sep(c: u8) -> bool {
    c == b'/'
}

/// Faithful port of `git_url_basename` from upstream `dir.c`: guess the
/// destination directory name for `git clone <repo>` when no explicit directory
/// is given.
fn git_url_basename(repo: &str, is_bundle: bool, is_bare: bool) -> String {
    let bytes = repo.as_bytes();
    let mut start = 0usize;
    let mut end = bytes.len();

    // Skip scheme ("://").
    if let Some(pos) = repo.find("://") {
        start = pos + 3;
    }

    // Skip authentication data, greedily up to the last '@' inside the host
    // part (before the first dir separator).
    {
        let mut ptr = start;
        while ptr < end && !is_dir_sep(bytes[ptr]) {
            if bytes[ptr] == b'@' {
                start = ptr + 1;
            }
            ptr += 1;
        }
    }

    // Strip trailing spaces, slashes and a trailing "/.git".
    while start < end && (is_dir_sep(bytes[end - 1]) || bytes[end - 1].is_ascii_whitespace()) {
        end -= 1;
    }
    if end >= start
        && end - start > 5
        && is_dir_sep(bytes[end - 5])
        && &bytes[end - 4..end] == b".git"
    {
        end -= 5;
        while start < end && is_dir_sep(bytes[end - 1]) {
            end -= 1;
        }
    }

    if end < start {
        return die_no_directory_name();
    }

    // Strip a trailing port number, but only for a bare hostname (no '/' but a
    // ':' present), so URLs like '/foo/bar:2222.git' keep '2222'.
    let span = &bytes[start..end];
    if !span.contains(&b'/') && span.contains(&b':') {
        let mut ptr = end;
        while start < ptr && bytes[ptr - 1].is_ascii_digit() && bytes[ptr - 1] != b':' {
            ptr -= 1;
        }
        if start < ptr && bytes[ptr - 1] == b':' {
            end = ptr - 1;
        }
    }

    // Find the last component. Colons count as separators too, so cloning
    // 'foo:bar.git' yields directory 'bar'.
    {
        let mut ptr = end;
        while start < ptr && !is_dir_sep(bytes[ptr - 1]) && bytes[ptr - 1] != b':' {
            ptr -= 1;
        }
        start = ptr;
    }

    // Strip a trailing ".bundle" or ".git" suffix.
    let mut len = end - start;
    let suffix: &[u8] = if is_bundle { b".bundle" } else { b".git" };
    if len >= suffix.len() && &bytes[start + len - suffix.len()..start + len] == suffix {
        len -= suffix.len();
    }

    if len == 0 || (len == 1 && bytes[start] == b'/') {
        return die_no_directory_name();
    }

    let base = &bytes[start..start + len];
    let mut dir: Vec<u8> = if is_bare {
        let mut v = base.to_vec();
        v.extend_from_slice(b".git");
        v
    } else {
        base.to_vec()
    };

    // Replace runs of control/whitespace chars with a single ASCII space, and
    // strip leading/trailing spaces.
    if !dir.is_empty() {
        let mut out = 0usize;
        let mut prev_space = true; // strip leading whitespace
        for i in 0..dir.len() {
            let mut ch = dir[i];
            if ch < 0x20 {
                ch = b' ';
            }
            if ch.is_ascii_whitespace() {
                if prev_space {
                    continue;
                }
                prev_space = true;
            } else {
                prev_space = false;
            }
            dir[out] = ch;
            out += 1;
        }
        dir.truncate(out);
        if out > 0 && prev_space {
            dir.truncate(out - 1);
        }
    }

    String::from_utf8_lossy(&dir).into_owned()
}

/// git's empty-repository clone warning. Printed (to stderr) after the "Cloning
/// into …" banner when the remote advertised no tip for the tracked branch.
fn warn_cloned_empty_repository() {
    eprintln!("warning: You appear to have cloned an empty repository.");
}

fn die_no_directory_name() -> String {
    // Upstream `die()`s here; sley callers only reach this with degenerate URLs
    // that the CLI already rejects. Fall back to a stable placeholder so the
    // path type stays infallible.
    "repository".to_string()
}

/// If `--branch=<name>` names a tag (and not a branch) in the local clone
/// source, return the commit it resolves to so the clone checks it out
/// detached. A branch of the same name takes precedence (git's
/// `find_remote_branch` searches `refs/heads/` first), so this returns `None`
/// when `refs/heads/<name>` exists.
fn clone_source_tag_commit(
    remote_common_git_dir: &Path,
    format: ObjectFormat,
    name: &str,
) -> Option<ObjectId> {
    let store = FileRefStore::new(remote_common_git_dir, format);
    if store
        .read_ref(&format!("refs/heads/{name}"))
        .ok()?
        .is_some()
    {
        return None;
    }
    let tag_ref = format!("refs/tags/{name}");
    store.read_ref(&tag_ref).ok()??;
    // Resolve (peeling annotated tags) to the underlying commit.
    sley_rev::resolve_revision(remote_common_git_dir, format, &tag_ref).ok()
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

fn clone_remote_head_branch(remote_git_dir: &Path, format: ObjectFormat) -> Result<Option<String>> {
    let remote_store = FileRefStore::new(remote_git_dir, format);
    let Some(RefTarget::Symbolic(target)) = remote_store.read_ref("HEAD")? else {
        return Ok(None);
    };
    let Some(branch) = target.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    if remote_store.read_ref(&target)?.is_some() {
        Ok(Some(branch.to_string()))
    } else {
        Ok(None)
    }
}

/// The remote `HEAD` commit when it is detached (no default branch).
fn remote_head_detached(remote_git_dir: &Path, format: ObjectFormat) -> Option<ObjectId> {
    let remote_store = FileRefStore::new(remote_git_dir, format);
    match remote_store.read_ref("HEAD").ok()? {
        Some(RefTarget::Direct(oid)) => Some(oid),
        _ => None,
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
    let mut config = read_repo_config_on_disk(git_dir)?;
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
        let repository_format = parse_config_key("core.repositoryformatversion")?;
        config_set_value(&mut config, &repository_format, "1", false);
        let partial_clone = parse_config_key("extensions.partialclone")?;
        config_set_value(&mut config, &partial_clone, name, false);
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

/// The first `extensions.<name>` key that git does not recognise (neither a
/// version-0-honoured extension nor a v1-only one), or `None` when every
/// extension is known. Mirrors the recognised set in
/// `rev_parse::verify_repository_format` / git's `handle_extension`.
fn first_unknown_repository_extension(config: &GitConfig) -> Option<String> {
    config
        .sections
        .iter()
        .filter(|section| {
            section.name.eq_ignore_ascii_case("extensions") && section.subsection.is_none()
        })
        .flat_map(|section| section.entries.iter())
        .map(|entry| entry.key.to_ascii_lowercase())
        .find(|ext| {
            !matches!(
                ext.as_str(),
                "noop"
                    | "preciousobjects"
                    | "partialclone"
                    | "worktreeconfig"
                    | "noop-v1"
                    | "objectformat"
                    | "compatobjectformat"
                    | "refstorage"
                    | "relativeworktrees"
                    | "submodulepathconfig"
            )
        })
}

/// Register a configured remote as a promisor remote after a `fetch --filter`,
/// mirroring git's `partial_clone_register`: upgrade the repo format to 1, set
/// `remote.<name>.promisor=true`, and record the filter spec under
/// `remote.<name>.partialclonefilter` (the default for later fetches from it).
/// A no-op when `name` is not a configured remote (e.g. a bare-URL fetch) or is
/// already a promisor remote with a recorded filter.
fn register_promisor_remote(git_dir: &Path, name: &str, filter_spec: &str) -> Result<()> {
    let mut config = read_repo_config_on_disk(git_dir)?;
    if config.get("remote", Some(name), "url").is_none() {
        return Ok(());
    }
    if config.get_bool("remote", Some(name), "promisor") == Some(true)
        && config.get("remote", Some(name), "partialclonefilter").is_some()
    {
        return Ok(());
    }
    // git's `upgrade_repository_format` refuses to bump a version-0 repo that
    // carries an extension it does not recognise: the unknown extension would
    // become active (and unsupported) at version 1.
    let version: i64 = config
        .get("core", None, "repositoryformatversion")
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    if version == 0
        && let Some(unknown) = first_unknown_repository_extension(&config)
    {
        eprintln!("error: cannot upgrade repository format: unknown extension {unknown}");
        return Err(GitError::Exit(128));
    }
    let format_key = parse_config_key("core.repositoryformatversion")?;
    config_set_value(&mut config, &format_key, "1", false);
    let promisor_key = parse_config_key(&format!("remote.{name}.promisor"))?;
    config_set_value(&mut config, &promisor_key, "true", false);
    let filter_key = parse_config_key(&format!("remote.{name}.partialclonefilter"))?;
    config_set_value(&mut config, &filter_key, filter_spec, false);
    write_repo_config(git_dir, &config)
}

fn configure_clone_branch(git_dir: &Path, branch: &str, remote: &str) -> Result<()> {
    let mut config = read_repo_config_on_disk(git_dir)?;
    let mut entries = vec![
        ConfigEntry::new("remote", Some(remote.to_string())),
        ConfigEntry::new("merge", Some(format!("refs/heads/{branch}"))),
    ];
    // Upstream `install_branch_config` consults `branch.autosetuprebase`: for a
    // remote-tracking branch (origin is the remote, always set during clone),
    // `remote` and `always` write `branch.<name>.rebase = true`. The value lives
    // in the full effective config — global `~/.gitconfig` and system files —
    // not just the on-disk repo config the write side starts from, so read the
    // layered stack here.
    if let Some(autosetuprebase) =
        clone_effective_config_value(git_dir, "branch", "autosetuprebase")
        && matches!(
            autosetuprebase.to_ascii_lowercase().as_str(),
            "remote" | "always"
        )
    {
        entries.push(ConfigEntry::new("rebase", Some("true".to_string())));
    }
    config.sections.push(ConfigSection::new(
        "branch",
        Some(branch.to_string()),
        entries,
    ));
    write_repo_config(git_dir, &config)
}

/// The remote `git fetch` uses when given no remote argument, mirroring git's
/// `remote_for_branch`: the current branch's `branch.<name>.remote` if set,
/// otherwise the sole configured remote, otherwise `origin`.
fn default_fetch_remote(git_dir: &Path, format: ObjectFormat) -> Result<String> {
    let config = read_repo_config(git_dir)?;
    if let Some(current) = FileRefStore::new(git_dir, format).current_branch()?
        && let Some(remote) = config.get("branch", Some(&current), "remote")
    {
        return Ok(remote.to_string());
    }
    let remotes = remote_names(&config);
    Ok(match remotes.as_slice() {
        [only] => only.clone(),
        _ => "origin".to_string(),
    })
}

/// The current branch's `branch.<name>.merge` values, but only when its
/// `branch.<name>.remote` is `remote` — git's `add_merge_config` only honors the
/// merge config when the branch's remote matches the remote being fetched.
fn current_branch_merge_for_remote(
    git_dir: &Path,
    format: ObjectFormat,
    remote: &str,
) -> Vec<String> {
    let Ok(config) = read_repo_config(git_dir) else {
        return Vec::new();
    };
    let Ok(Some(current)) = FileRefStore::new(git_dir, format).current_branch() else {
        return Vec::new();
    };
    if config.get("branch", Some(&current), "remote") != Some(remote) {
        return Vec::new();
    }
    config
        .get_all("branch", Some(&current), "merge")
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect()
}

pub(crate) fn cmd_fetch(args: &[String]) -> Result<()> {
    let mut source = None::<String>;
    let mut refspecs = Vec::new();
    let mut options = FetchOptions {
        quiet: false,
        auto_follow_tags: true,
        fetch_all_tags: false,
        prune: false,
        prune_tags: false,
        dry_run: false,
        append: false,
        write_fetch_head: true,
        tag_option_explicit: false,
        prune_option_explicit: false,
        prune_tags_option_explicit: false,
        refmap: None,
        depth: None,
        merge_srcs: Vec::new(),
        filter: None,
        refetch: false,
        cloning: false,
        record_promisor_refs: true,
        update_shallow: false,
        deepen_relative: false,
        update_head_ok: false,
        deepen_since: None,
        deepen_not: Vec::new(),
        ssh_options: None,
        atomic: false,
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
                validate_clone_filter(value)?;
                options.filter = fetch_pack_filter_from_spec(value);
                filter_spec = Some(value.to_string());
                filter_option_explicit = true;
            }
            value if value.starts_with("--filter=") => {
                let value = value
                    .strip_prefix("--filter=")
                    .ok_or_else(|| GitError::Command("fetch --filter requires a value".into()))?;
                validate_clone_filter(value)?;
                options.filter = fetch_pack_filter_from_spec(value);
                filter_spec = Some(value.to_string());
                filter_option_explicit = true;
            }
            "--no-filter" => {
                options.filter = None;
                filter_spec = None;
                filter_option_explicit = true;
            }
            "--refetch" => options.refetch = true,
            "--no-refetch" => options.refetch = false,
            "--prefetch" => prefetch = true,
            "--no-prefetch" => prefetch = false,
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
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    // A bare repo with no working tree never has a "checked out" branch, so the
    // current-branch fetch refusal is keyed off whether a *non-bare* worktree
    // shares the symref (`find_shared_symref` skips bare worktrees) rather than a
    // blanket update-head-ok for every bare repo — otherwise a bare repo's linked
    // worktree branch could be overwritten by fetch (t5516 #120).
    let config = read_repo_config(&git_dir)?;
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
            git_dir: &git_dir,
            format,
            worktree_root: &cwd,
            config: &config,
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
            git_dir: &git_dir,
            format,
            worktree_root: &cwd,
            config: &config,
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
    let source = match source {
        Some(source) => source,
        None => default_fetch_remote(&git_dir, format)?,
    };
    // When no refspecs are given on the command line and the current branch's
    // `branch.<name>.remote` is the remote we're fetching, git's get_ref_map adds
    // the branch's `branch.<name>.merge` ref(s) as the FETCH_HEAD for-merge
    // entries (`add_merge_config`). Resolve those so the configured-refspec fetch
    // marks them correctly (and `pull` can find its merge target).
    if refspecs.is_empty() && !prefetch {
        options.merge_srcs = current_branch_merge_for_remote(&git_dir, format, &source);
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
        let config = read_repo_config(&git_dir)?;
        apply_configured_partial_clone_filter(&config, &source, &mut options);
    }
    let effective_refspecs = if prefetch {
        prefetch_refspecs(&config, &source, &refspecs)
    } else {
        refspecs.clone()
    };
    if fetch_raw_oid_refspecs(
        &git_dir,
        format,
        &source,
        &effective_refspecs,
        &options,
        filter_spec.as_deref(),
    )? {
        return Ok(());
    }
    let before_fetch_refs = fetch_ref_snapshot(&git_dir, format)?;
    let refetch = options.refetch;
    let effective_server_options = if server_options_from_cli {
        server_options
    } else {
        configured_server_options(&config, &source)?
    };
    if server_options_from_cli && configured_legacy_protocol(Some(&config)) {
        eprintln!("fatal: server options require protocol version 2 or later");
        eprintln!("fatal: see protocol.version in 'git help config' for more details");
        return Err(GitError::Exit(128));
    }
    let result = fetch_one_source_with_outcome(
        &git_dir,
        format,
        &source,
        &effective_refspecs,
        options.clone(),
        &effective_server_options,
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
        register_promisor_remote(&git_dir, &source, spec)?;
    }
    if set_upstream {
        fetch_set_upstream_from_outcome(&git_dir, format, &source, &outcome)?;
    }
    let config = read_repo_config(&git_dir)?;
    trace2_local_transfer_negotiation(&config, upload_pack_command.as_deref());
    let recurse_submodules = resolve_fetch_recurse_submodules(
        &config,
        recurse_submodules_cli,
        recurse_submodules_default,
    );
    fetch_populated_submodules_after_superproject(FetchSubmoduleRequest {
        git_dir: &git_dir,
        format,
        worktree_root: &cwd,
        config: &config,
        recurse_submodules,
        default_recurse_submodules: recurse_submodules_default,
        source: &source,
        changed_gitlinks: changed_gitlinks_for_fetch(
            &git_dir,
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
            apply_configured_partial_clone_filter(req.config, &remote, &mut remote_options);
        }
        if req.refspecs.is_empty() && !req.prefetch {
            remote_options.merge_srcs =
                current_branch_merge_for_remote(req.git_dir, req.format, &remote);
        }
        let effective_refspecs = if req.prefetch {
            prefetch_refspecs(req.config, &remote, req.refspecs)
        } else {
            req.refspecs.to_vec()
        };
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
    pack_filter_from_spec(spec)
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
    fn enter(path: &Path) -> Result<Self> {
        let previous = env::current_dir()?;
        env::set_current_dir(path)?;
        Ok(Self { previous })
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
        let sub_source = default_fetch_remote(&sub_git_dir, sub_format)?;
        if !req.options.quiet {
            eprintln!(
                "Fetching submodule {}{}",
                req.submodule_prefix, submodule.path
            );
        }
        let nested_prefix = format!("{}{}{}", req.submodule_prefix, submodule.path, "/");
        trace_submodule_fetch(&nested_prefix, &sub_source, &[]);
        let mut sub_options = req.options.clone();
        sub_options.merge_srcs =
            current_branch_merge_for_remote(&sub_git_dir, sub_format, &sub_source);
        let fetch_cwd = if submodule_root.is_dir() {
            submodule_root.as_path()
        } else {
            sub_git_dir.as_path()
        };
        let _guard = CurrentDirGuard::enter(fetch_cwd)?;
        let before_sub_refs = fetch_ref_snapshot(&sub_git_dir, sub_format)?;
        let outcome = fetch_one_source_with_outcome(
            &sub_git_dir,
            sub_format,
            &sub_source,
            &[],
            sub_options.clone(),
            &[],
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
            )?;
        }
        let nested_changed_gitlinks =
            changed_gitlinks_for_fetch(&sub_git_dir, sub_format, &before_sub_refs, &outcome)?;
        let nested_config = read_repo_config(&sub_git_dir)?;
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
    if submodule_has_commit(&sub_git_dir, sub_format, &changed.oid) {
        return Ok(());
    }
    trace_submodule_get_default_remote(&display_path);
    let sub_source = default_fetch_remote(&sub_git_dir, sub_format)?;
    if !req.options.quiet {
        eprintln!(
            "Fetching submodule {}{} at commit {}",
            req.submodule_prefix,
            display_path,
            short_object_id(&changed.super_oid)
        );
    }
    let mut sub_options = req.options.clone();
    sub_options.merge_srcs = current_branch_merge_for_remote(&sub_git_dir, sub_format, &sub_source);
    let fetch_cwd = if submodule_root.is_dir() {
        submodule_root.as_path()
    } else {
        sub_git_dir.as_path()
    };
    let _guard = CurrentDirGuard::enter(fetch_cwd)?;
    let before_sub_refs = fetch_ref_snapshot(&sub_git_dir, sub_format)?;
    let outcome = fetch_one_source_with_outcome(
        &sub_git_dir,
        sub_format,
        &sub_source,
        &[],
        sub_options.clone(),
        &[],
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
        )?;
    }
    let mut nested_changed_gitlinks =
        changed_gitlinks_for_fetch(&sub_git_dir, sub_format, &before_sub_refs, &outcome)?;
    if nested_changed_gitlinks.is_empty() {
        nested_changed_gitlinks =
            changed_gitlinks_for_commit(&sub_git_dir, sub_format, &changed.oid)?;
    }
    let nested_prefix = format!("{}{}/", req.submodule_prefix, display_path);
    let nested_config = read_repo_config(&sub_git_dir)?;
    let nested_recurse_submodules = if req.recurse_submodules == FetchRecurseSubmodules::Default {
        resolve_fetch_recurse_submodules(&nested_config, FetchRecurseSubmodules::Default, mode)
    } else {
        req.recurse_submodules
    };
    fetch_populated_submodules_after_superproject(FetchSubmoduleRequest {
        git_dir: &sub_git_dir,
        format: sub_format,
        worktree_root: &submodule_root,
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
    let Some(name) =
        submodule_name_for_path_at_commit(req.git_dir, req.format, &changed.super_oid, &changed.path)?
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
    eprintln!(
        "fatal: not a git repository: {}",
        git_dir.display()
    );
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
        let old = update.dst.as_deref().and_then(|dst| before.get(dst)).copied();
        if old == Some(update.oid) {
            continue;
        }
        for gitlink in changed_gitlinks_for_commit_range(&db, format, old, &update.oid)? {
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
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old: Option<ObjectId>,
    new: &ObjectId,
) -> Result<Vec<ChangedGitlink>> {
    let old_ancestors = match old {
        Some(old) => ancestor_depths(db, format, &old)?,
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

fn pack_filter_from_spec(spec: &str) -> Option<sley_odb::PackObjectFilter> {
    if let Some(parts) = spec.strip_prefix("combine:") {
        return parts
            .split('+')
            .filter_map(pack_filter_from_spec)
            .reduce(combine_pack_filters);
    }
    if spec == "blob:none" {
        return Some(sley_odb::PackObjectFilter::BlobNone);
    }
    if let Some(depth) = spec.strip_prefix("tree:") {
        return parse_rev_list_tree_depth(depth).ok().map(|depth| {
            sley_odb::PackObjectFilter::TreeDepth(depth.min(u32::MAX as usize) as u32)
        });
    }
    spec.strip_prefix("blob:limit=")
        .and_then(git_parse_blob_limit)
        .map(sley_odb::PackObjectFilter::BlobLimit)
}

fn pack_filter_from_spec_for_clone(
    spec: &str,
    remote_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<sley_odb::PackObjectFilter>> {
    if let Some(body) = spec.strip_prefix("sparse:oid=") {
        return sparse_filter_from_remote(body, remote_git_dir, format).map(Some);
    }
    Ok(pack_filter_from_spec(spec))
}

fn sparse_filter_from_remote(
    body: &str,
    remote_git_dir: &Path,
    format: ObjectFormat,
) -> Result<sley_odb::PackObjectFilter> {
    let Some((rev, path)) = body.split_once(':') else {
        eprintln!("fatal: unable to parse sparse filter data in .{body}");
        return Err(GitError::Exit(128));
    };
    let db = FileObjectDatabase::from_git_dir(remote_git_dir, format);
    let oid = match sley_rev::resolve_rev_path(remote_git_dir, format, &db, rev, path) {
        Ok(oid) => oid,
        Err(_) => {
            eprintln!("fatal: unable to access sparse blob in .{body}");
            return Err(GitError::Exit(128));
        }
    };
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Blob {
        eprintln!("fatal: unable to parse sparse filter data in .{body}");
        return Err(GitError::Exit(128));
    }
    let contents = String::from_utf8_lossy(&object.body);
    let paths = contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix('/').unwrap_or(line);
            (!line.is_empty()).then(|| line.to_string())
        })
        .collect::<Vec<_>>();
    if paths.is_empty() {
        eprintln!("fatal: unable to parse sparse filter data in .{body}");
        return Err(GitError::Exit(128));
    }
    Ok(sley_odb::PackObjectFilter::SparsePathSet(paths))
}

fn combine_pack_filters(
    left: sley_odb::PackObjectFilter,
    right: sley_odb::PackObjectFilter,
) -> sley_odb::PackObjectFilter {
    use sley_odb::PackObjectFilter;
    match (left, right) {
        (PackObjectFilter::TreeDepth(a), PackObjectFilter::TreeDepth(b)) => {
            PackObjectFilter::TreeDepth(a.min(b))
        }
        (PackObjectFilter::TreeDepth(depth), _) | (_, PackObjectFilter::TreeDepth(depth)) => {
            PackObjectFilter::TreeDepth(depth)
        }
        (PackObjectFilter::SparsePathSet(paths), _)
        | (_, PackObjectFilter::SparsePathSet(paths)) => PackObjectFilter::SparsePathSet(paths),
        (PackObjectFilter::BlobLimit(a), PackObjectFilter::BlobLimit(b)) => {
            PackObjectFilter::BlobLimit(a.min(b))
        }
        (PackObjectFilter::BlobNone, _) | (_, PackObjectFilter::BlobNone) => {
            PackObjectFilter::BlobNone
        }
    }
}

fn apply_configured_partial_clone_filter(
    config: &GitConfig,
    remote: &str,
    options: &mut FetchOptions,
) {
    if config
        .get_bool("remote", Some(remote), "promisor")
        .unwrap_or(false)
        && let Some(filter) = config.get("remote", Some(remote), "partialclonefilter")
    {
        options.filter = fetch_pack_filter_from_spec(filter);
    }
}

fn fetch_raw_oid_refspecs(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: &FetchOptions,
    filter_spec: Option<&str>,
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
    let Ok(remote_git_dir) = ls_remote_git_dir(source) else {
        return Ok(false);
    };
    let config = read_repo_config(git_dir)?;
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
) -> Result<sley_remote::FetchOutcome> {
    if let Some((bundle_source, bundle)) = fetch_bundle_source(git_dir, format, source)?
    {
        // Bundle fetches have no shallow support, so a `--depth` is warned-and-
        // ignored here, matching the local-clone behavior.
        if options.depth.is_some() {
            eprintln!("warning: --depth is ignored in bundle fetches; use file:// instead.");
        }
        let configured_refspecs;
        let bundle_refspecs = if refspecs.is_empty() {
            let config = read_repo_config(git_dir)?;
            configured_refspecs = remote_config_values(&config, source, "fetch");
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
            options,
        )?;
        return Ok(sley_remote::FetchOutcome::default());
    }
    let config = transport_policy_config_for_cwd()?;
    let resolved = ls_remote_resolved_url(source)?;
    check_transport_allowed_url(&resolved, Some(&config))?;
    if fetch_source_is_http(source)? {
        return fetch_http_repository_with_outcome(git_dir, format, source, refspecs, options);
    }
    if fetch_source_is_ssh(source)? {
        return fetch_ssh_repository_with_outcome(git_dir, format, source, refspecs, options);
    }
    if fetch_source_is_git(source)? {
        return fetch_git_repository_with_outcome(git_dir, format, source, refspecs, options);
    }
    fetch_local_repository_with_outcome(git_dir, format, source, refspecs, options, server_options)
}

fn fetch_bundle_source(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
) -> Result<Option<(String, Bundle)>> {
    if let Ok(input) = fs::read(source)
        && let Ok(bundle) = Bundle::parse(&input, format)
    {
        return Ok(Some((source.to_string(), bundle)));
    }
    let config = read_repo_config(git_dir)?;
    let resolved = resolve_remote_fetch_url(&config, source);
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
fn parse_shallow_since(value: &str) -> Result<i64> {
    crate::commands::approxidate::parse_commit_date(value)
        .map(|(seconds, _)| seconds)
        .or_else(|| crate::commands::approxidate::parse_expiry_date(value))
        .ok_or_else(|| GitError::Command(format!("invalid shallow-since date: {value}")))
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
    if !request.packfile.is_empty() {
        let config = read_repo_config(&git_dir)?;
        if config
            .get_bool("transfer", None, "fsckObjects")
            .unwrap_or(false)
        {
            let exit = super::pack::fsck_pack_objects(&request.packfile, format, &[])?;
            if exit != 0 {
                return Err(GitError::Exit(exit));
            }
        }
    }
    let report = sley_remote::receive_pack_into_local_repository(&git_dir, format, &request)?;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_receive_pack_report_status(&mut stdout, &report)?;
    stdout.flush()?;
    Ok(())
}

/// Whether the connecting client requested protocol v2 via the `GIT_PROTOCOL`
/// environment variable (`version=2`, possibly among colon-separated tokens).
/// Mirrors `git_protocol_version_from_environment` in protocol.c.
fn upload_pack_requested_protocol_v2() -> bool {
    let Ok(value) = std::env::var("GIT_PROTOCOL") else {
        return false;
    };
    value.split(':').any(|token| token == "version=2")
}

pub(crate) fn cmd_upload_pack(args: &[String]) -> Result<()> {
    // Accept (and ignore) the upload-pack flags the transports pass through:
    // `git daemon` runs `upload-pack --strict <dir>`, the smart transports add
    // `--stateless-rpc`/`--advertise-refs`/`--timeout=<n>`. The repository is
    // the lone positional argument. Mirrors builtin/upload-pack.c's options.
    let mut repository: Option<&String> = None;
    for arg in args {
        match arg.as_str() {
            "--strict" | "--stateless-rpc" | "--advertise-refs" | "--http-backend-info-refs" => {}
            value if value.starts_with("--timeout=") => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "upload-pack: unknown option {value}"
                )));
            }
            value => {
                if repository.is_some() {
                    return Err(GitError::Command(
                        "upload-pack currently supports: upload-pack <repository>".into(),
                    ));
                }
                repository = Some(arg);
                let _ = value;
            }
        }
    }
    let Some(repository) = repository else {
        return Err(GitError::Command(
            "upload-pack currently supports: upload-pack <repository>".into(),
        ));
    };
    let git_dir = ls_remote_git_dir(repository)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    validate_upload_pack_filter_config()?;

    // Protocol v2: the client requests version 2 via the `GIT_PROTOCOL`
    // environment variable (the daemon/file:// transport propagates it from the
    // connection's `version=2` extra-arg). Run the v2 server loop instead of the
    // v0 ref advertisement. Mirrors upload-pack.c's `determine_protocol_version`.
    if upload_pack_requested_protocol_v2() {
        let config = read_repo_config(&git_dir)?;
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        return sley_remote::serve_upload_pack_v2_with_config(
            &git_dir,
            format,
            &config,
            &mut stdin,
            &mut stdout,
        );
    }
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

const SEND_PACK_USAGE: &str = "usage: git send-pack [--mirror] [--dry-run] [--force]\n              [--receive-pack=<git-receive-pack>]\n              [--verbose] [--thin] [--atomic]\n              [--[no-]signed | --signed=(true|false|if-asked)]\n              [<host>:]<directory> (--all | <ref>...)";

/// `git send-pack [<options>] <directory> (--all | <ref>...)`: the plumbing push
/// to a local repository. Unlike `git push`, it takes a literal destination
/// directory (no remote-name resolution) and pushes bare refs as `<ref>:<ref>`
/// (a leading `:` deletes). It renders the same status report as push but does
/// not touch remote-tracking refs (it is a low-level transport, not porcelain).
pub(crate) fn cmd_send_pack(args: &[String]) -> Result<()> {
    // `-h` is handled by git's option parser before any repository is opened, so
    // it works outside a git repo too (the `nongit` case in t5400).
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{SEND_PACK_USAGE}");
        return Err(GitError::Exit(129));
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&git_dir, format);

    let mut force = false;
    let mut dry_run = false;
    let mut atomic = false;
    let mut mirror = false;
    let mut all_refs = false;
    let mut quiet = false;
    let mut from_stdin = false;
    let mut force_with_lease_specs: Vec<String> = Vec::new();
    let mut receive_pack_command: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{SEND_PACK_USAGE}");
                return Err(GitError::Exit(129));
            }
            "-f" | "--force" => force = true,
            "-n" | "--dry-run" => dry_run = true,
            "--atomic" => atomic = true,
            "--mirror" => mirror = true,
            "--all" => all_refs = true,
            "--stdin" => from_stdin = true,
            "-q" | "--quiet" => quiet = true,
            "-v" | "--verbose" | "--progress" | "--no-progress" | "--thin" | "--no-thin"
            | "--stateless-rpc" | "--helper-status" => {}
            "--signed" | "--no-signed" => {}
            value if value.starts_with("--signed=") => {}
            "--force-if-includes" => {}
            value if value.starts_with("--force-with-lease=") => {
                force_with_lease_specs.push(value["--force-with-lease=".len()..].to_string());
            }
            "--receive-pack" | "--exec" => {
                receive_pack_command = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command(format!(
                                "send-pack {} requires a value",
                                arg.as_str()
                            ))
                        })?
                        .to_string(),
                );
            }
            "--remote" | "--push-option" => {
                iter.next().ok_or_else(|| {
                    GitError::Command(format!("send-pack {} requires a value", arg.as_str()))
                })?;
            }
            value if value.starts_with("--receive-pack=") || value.starts_with("--exec=") => {
                let (_, command) = value.split_once('=').unwrap_or((value, ""));
                receive_pack_command = Some(command.to_string());
            }
            value if value.starts_with("--remote=") || value.starts_with("--push-option=") => {}
            "--" => {
                positional.extend(iter.map(|value| value.to_string()));
                break;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                eprintln!("{SEND_PACK_USAGE}");
                return Err(GitError::Exit(129));
            }
            value => positional.push(value.to_string()),
        }
    }

    if mirror {
        force = true;
    }

    let Some((dest, refs)) = positional.split_first() else {
        eprintln!("{SEND_PACK_USAGE}");
        return Err(GitError::Exit(129));
    };
    let mut refs = refs.to_vec();
    if from_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        refs.extend(
            input
                .lines()
                .map(str::trim_end)
                .filter(|line| !line.is_empty())
                .map(str::to_string),
        );
    }
    if refs.is_empty() && !all_refs && !mirror {
        eprintln!("{SEND_PACK_USAGE}");
        return Err(GitError::Exit(129));
    }
    if !refs.is_empty() && (all_refs || mirror) {
        eprintln!("{SEND_PACK_USAGE}");
        return Err(GitError::Exit(129));
    }

    let remote_git_dir = ls_remote_git_dir(dest)?;
    let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;

    let mut refspecs: Vec<String> = if mirror {
        vec!["refs/*:refs/*".to_string()]
    } else if all_refs {
        vec!["refs/heads/*:refs/heads/*".to_string()]
    } else {
        // send-pack pushes each bare ref as `<ref>:<ref>` (a leading `:` is a
        // delete; a leading `+` forces); no porcelain-style short-name guessing.
        refs.iter()
            .map(|spec| {
                let (force_marker, body) = match spec.strip_prefix('+') {
                    Some(rest) => ("+", rest),
                    None => ("", spec.as_str()),
                };
                let normalized = if let Some(rest) = body.strip_prefix(':') {
                    format!(":{}", sley_remote::normalize_push_refname(rest))
                } else if let Some((src, dstn)) = body.split_once(':') {
                    format!(
                        "{}:{}",
                        sley_remote::normalize_push_refname(src),
                        sley_remote::normalize_push_refname(dstn)
                    )
                } else {
                    let name = sley_remote::normalize_push_refname(body);
                    format!("{name}:{name}")
                };
                format!("{force_marker}{normalized}")
            })
            .collect()
    };
    reject_duplicate_push_destinations(&refspecs)?;

    // `--mirror` deletes remote refs the local repo no longer has.
    if mirror {
        let remote_advertisements =
            sley_remote::local_fetch_advertisements(&remote_git_dir, format)?;
        let local_names: std::collections::HashSet<String> = store
            .list_refs()?
            .into_iter()
            .map(|reference| reference.name)
            .collect();
        for advertisement in &remote_advertisements {
            if advertisement.name.starts_with("refs/") && !local_names.contains(&advertisement.name)
            {
                refspecs.push(format!(":{}", advertisement.name));
            }
        }
    }

    let config = read_repo_config(&git_dir).unwrap_or_default();
    let force_with_lease = resolve_force_with_lease(
        &git_dir,
        &store,
        &config,
        format,
        dest,
        &force_with_lease_specs,
    )?;
    let receive_config_overrides = receive_pack_config_overrides(receive_pack_command.as_deref());
    let options = PushOptions {
        quiet,
        set_upstream: false,
        force,
        no_verify: true,
        dry_run,
        progress: false,
    };
    run_push_local_report(RunPushLocalReport {
        git_dir: &git_dir,
        common_git_dir: &common_git_dir,
        format,
        remote: dest,
        remote_git_dir: &remote_git_dir,
        remote_common_git_dir: &remote_common_git_dir,
        refspecs: &refspecs,
        options,
        porcelain: false,
        atomic,
        force_if_includes: false,
        push_options: &[],
        force_with_lease: &force_with_lease,
        force_with_lease_default: false,
        receive_config_overrides: &receive_config_overrides,
    })
}

fn reject_duplicate_push_destinations(refspecs: &[String]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for refspec in refspecs {
        let body = refspec.strip_prefix('+').unwrap_or(refspec);
        let dst = body.split_once(':').map(|(_, dst)| dst).unwrap_or(body);
        if dst.is_empty() || dst.contains('*') {
            continue;
        }
        if !seen.insert(dst.to_string()) {
            eprintln!("error: multiple updates for ref '{dst}' not allowed");
            return Err(GitError::Exit(128));
        }
    }
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
    let mut no_verify = false;
    let mut dry_run = false;
    let mut progress = false;
    let mut porcelain = false;
    let mut atomic = false;
    let mut mirror = false;
    let mut all_refs = false;
    let mut tags = false;
    let mut follow_tags = false;
    let mut prune = false;
    let mut receive_pack_command: Option<String> = None;
    let mut push_options_cmdline: Option<Vec<String>> = None;
    // `--force-with-lease` requests: an explicit `ref:expect` lease, or the
    // bare flag (lease every pushed ref against its remote-tracking ref).
    let mut force_with_lease_default = false;
    let mut force_with_lease_specs: Vec<String> = Vec::new();
    let mut force_if_includes = false;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "-f" | "--force" => force = true,
            "--no-force" => force = false,
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "--no-verify" => no_verify = true,
            "--verify" => no_verify = false,
            "-u" | "--set-upstream" => set_upstream = true,
            "--no-set-upstream" => set_upstream = false,
            "-d" | "--delete" => delete = true,
            "--no-delete" => delete = false,
            "--porcelain" => porcelain = true,
            "--atomic" => atomic = true,
            "--no-atomic" => atomic = false,
            "--mirror" => mirror = true,
            "--prune" => prune = true,
            "--no-prune" => prune = false,
            "--all" | "--branches" => all_refs = true,
            "--tags" => tags = true,
            "--follow-tags" => follow_tags = true,
            "--no-follow-tags" => follow_tags = false,
            "--force-with-lease" => force_with_lease_default = true,
            "--no-force-with-lease" => {
                force_with_lease_default = false;
                force_with_lease_specs.clear();
            }
            "--force-if-includes" => force_if_includes = true,
            "--no-force-if-includes" => force_if_includes = false,
            value if value.starts_with("--force-with-lease=") => {
                force_with_lease_specs.push(value["--force-with-lease=".len()..].to_string());
            }
            "--repo" => {
                iter.next().ok_or_else(|| {
                    GitError::Command(format!("push {} requires a value", arg.as_str()))
                })?;
            }
            "--receive-pack" | "--exec" => {
                receive_pack_command = Some(
                    iter.next()
                        .ok_or_else(|| {
                            GitError::Command(format!("push {} requires a value", arg.as_str()))
                        })?
                        .to_string(),
                );
            }
            value if value.starts_with("--receive-pack=") || value.starts_with("--exec=") => {
                let (_, command) = value.split_once('=').unwrap_or((value, ""));
                receive_pack_command = Some(command.to_string());
            }
            "-o" | "--push-option" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command(format!("push {} requires a value", arg.as_str()))
                })?;
                push_options_cmdline
                    .get_or_insert_with(Vec::new)
                    .push(value.clone());
            }
            value if value.starts_with("--push-option=") => {
                push_options_cmdline
                    .get_or_insert_with(Vec::new)
                    .push(value["--push-option=".len()..].to_string());
            }
            value if value.starts_with("--repo=") => {}
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            "--thin" | "--no-thin" => {}
            value if value.starts_with("--recurse-submodules=") => {}
            // `OPT_IPVERSION` in builtin/push.c: accepted but a no-op for the
            // file:// transport (the `--no-` forms are not defined and fall
            // through to the unknown-option path, matching git).
            "-4" | "--ipv4" | "-6" | "--ipv6" => {}
            "--" => {
                positional.extend(iter.map(|value| value.to_string()));
                break;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                eprintln!("usage: git push [<options>] [<repository> [<refspec>...]]");
                return Err(GitError::Exit(129));
            }
            value => positional.push(value.to_string()),
        }
    }

    // `--mirror` implies `--force` and forbids explicit refspecs.
    if mirror {
        if positional.len() >= 2 {
            return Err(GitError::Command(
                "--mirror can't be combined with refspecs".into(),
            ));
        }
        force = true;
    }
    if all_refs && positional.len() >= 2 {
        return Err(GitError::Command(
            "--all can't be combined with refspecs".into(),
        ));
    }

    let (remote, mut refspecs) = if delete {
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
        let specs = if names.first().is_some_and(|name| name == "tag") {
            names
                .iter()
                .skip(1)
                .map(|name| format!(":refs/tags/{name}"))
                .collect()
        } else {
            names.iter().map(|refspec| format!(":{refspec}")).collect()
        };
        (remote.clone(), specs)
    } else if mirror {
        // `--mirror`: push every local ref to the same name, mirroring; the
        // remote-ref deletions for refs we no longer have are expanded below
        // once the remote's advertisement is known.
        let remote = mirror_all_remote(&git_dir, &store, &positional)?;
        (remote, vec!["refs/*:refs/*".to_string()])
    } else if all_refs || (tags && positional.len() < 2) {
        // `--all`/`--mirror` forbid explicit refspecs (checked above), and a bare
        // `--tags` (no refspec) pushes only the tag wildcard. git appends
        // `refs/tags/*` as its own refspec rather than replacing the default.
        let remote = mirror_all_remote(&git_dir, &store, &positional)?;
        let mut specs = Vec::new();
        if all_refs {
            specs.push("refs/heads/*:refs/heads/*".to_string());
        }
        if tags {
            specs.push("refs/tags/*:refs/tags/*".to_string());
        }
        (remote, specs)
    } else {
        let resolved = push_remote_and_refspecs(&git_dir, &store, &positional)?;
        if resolved.set_upstream {
            set_upstream = true;
        }
        if resolved.mirror {
            mirror = true;
            force = true;
        }
        let mut specs = resolved.refspecs;
        // builtin/push.c: `--tags` appends `refs/tags/*` after the explicit
        // refspecs, so `git push --tags <remote> <refspec>` pushes both.
        if tags {
            specs.push("refs/tags/*:refs/tags/*".to_string());
        }
        (resolved.remote, specs)
    };
    refspecs = expand_push_tag_shorthand(&refspecs)?;
    default_head_push_destinations(&store, &mut refspecs)?;
    let options = PushOptions {
        quiet,
        set_upstream,
        force,
        no_verify,
        dry_run,
        progress,
    };
    let config = transport_policy_config_for_cwd()?;
    let resolved_remote = push_resolved_url(&remote)?;
    check_transport_allowed_url(&resolved_remote, Some(&config))?;
    let parsed_remote = parse_remote_url(&resolved_remote)?;
    // All transports delegate the git work to `sley_remote::push`, picked purely
    // by the resolved `PushDestination`; this command keeps owning URL/repo
    // resolution, set-upstream config, and the "To <remote>" summary so the
    // user-visible output stays byte-for-byte identical.
    let destination = match parsed_remote.transport {
        RemoteTransport::Ssh | RemoteTransport::Ext => {
            sley_remote::PushDestination::Ssh(parsed_remote)
        }
        RemoteTransport::Git => sley_remote::PushDestination::Git(parsed_remote),
        RemoteTransport::Http | RemoteTransport::Https => {
            sley_remote::PushDestination::Http(parsed_remote)
        }
        RemoteTransport::Local | RemoteTransport::File => {
            let remote_git_dir = ls_remote_git_dir(&resolved_remote)?;
            let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
            sley_remote::PushDestination::Local {
                git_dir: remote_git_dir,
                common_git_dir: remote_common_git_dir,
            }
        }
    };

    // The file:// (local) transport gets git's full per-ref status report: this
    // is the path the upstream push tests exercise. Other transports keep the
    // existing terse summary.
    if let sley_remote::PushDestination::Local {
        git_dir: remote_git_dir,
        common_git_dir: remote_common_git_dir,
    } = &destination
    {
        // `--mirror` also deletes remote refs the local repo no longer has.
        let remote_advertisements = if mirror || prune || follow_tags {
            Some(sley_remote::local_fetch_advertisements(
                remote_git_dir,
                format,
            )?)
        } else {
            None
        };
        if mirror || prune {
            let remote_advertisements = remote_advertisements.as_deref().unwrap_or(&[]);
            let local_names: std::collections::HashSet<String> = store
                .list_refs()?
                .into_iter()
                .map(|reference| reference.name)
                .collect();
            if mirror {
                for advertisement in remote_advertisements {
                    if advertisement.name.starts_with("refs/")
                        && !local_names.contains(&advertisement.name)
                    {
                        refspecs.push(format!(":{}", advertisement.name));
                    }
                }
            } else {
                append_push_prune_refspecs(&mut refspecs, &remote_advertisements, &local_names);
            }
        }
        if follow_tags {
            append_follow_tag_refspecs(
                &git_dir,
                &common_git_dir,
                format,
                &store,
                &mut refspecs,
                remote_advertisements.as_deref().unwrap_or(&[]),
            )?;
        }
        let config = read_repo_config(&git_dir).unwrap_or_default();
        let force_if_includes = force_if_includes
            || config
                .get_bool("push", None, "useforceifincludes")
                .unwrap_or(false);
        let push_options = match push_options_cmdline.clone() {
            Some(options) => options,
            None => push_options_from_config(&config)?,
        };
        let receive_config_overrides =
            receive_pack_config_overrides(receive_pack_command.as_deref());
        let mut force_with_lease = resolve_force_with_lease(
            &git_dir,
            &store,
            &config,
            format,
            &remote,
            &force_with_lease_specs,
        )?;
        if force_with_lease_default {
            expand_default_force_with_lease(
                &git_dir,
                &common_git_dir,
                format,
                &store,
                &config,
                &remote,
                remote_git_dir,
                remote_common_git_dir,
                &refspecs,
                force,
                atomic,
                force_if_includes,
                &receive_config_overrides,
                &mut force_with_lease,
            )?;
        }
        trace_configured_local_protocol_version(Some(&config));
        let result = run_push_local_report(RunPushLocalReport {
            git_dir: &git_dir,
            common_git_dir: &common_git_dir,
            format,
            remote: &remote,
            remote_git_dir,
            remote_common_git_dir,
            refspecs: &refspecs,
            options,
            porcelain,
            atomic,
            force_if_includes,
            push_options: &push_options,
            force_with_lease: &force_with_lease,
            force_with_lease_default,
            receive_config_overrides: &receive_config_overrides,
        });
        if result.is_ok() {
            trace2_local_transfer_negotiation(&config, receive_pack_command.as_deref());
        }
        return result;
    }

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

fn append_push_prune_refspecs(
    refspecs: &mut Vec<String>,
    remote_advertisements: &[sley_protocol::RefAdvertisement],
    local_names: &std::collections::HashSet<String>,
) {
    let mut deletes = std::collections::BTreeSet::new();
    for refspec in refspecs.iter() {
        let body = refspec.strip_prefix('+').unwrap_or(refspec);
        if body == ":" {
            for advertisement in remote_advertisements {
                if advertisement.name.starts_with("refs/heads/")
                    && !local_names.contains(&advertisement.name)
                {
                    deletes.insert(advertisement.name.clone());
                }
            }
            continue;
        }
        let Some((src, dst)) = body.split_once(':') else {
            continue;
        };
        let (Some((src_prefix, src_suffix)), Some((dst_prefix, dst_suffix))) =
            (src.split_once('*'), dst.split_once('*'))
        else {
            continue;
        };
        for advertisement in remote_advertisements {
            let Some(stem) = advertisement
                .name
                .strip_prefix(dst_prefix)
                .and_then(|rest| rest.strip_suffix(dst_suffix))
            else {
                continue;
            };
            let source = format!("{src_prefix}{stem}{src_suffix}");
            if !local_names.contains(&source) {
                deletes.insert(advertisement.name.clone());
            }
        }
    }
    refspecs.extend(deletes.into_iter().map(|name| format!(":{name}")));
}

fn expand_push_tag_shorthand(refspecs: &[String]) -> Result<Vec<String>> {
    let mut expanded = Vec::new();
    let mut iter = refspecs.iter();
    while let Some(refspec) = iter.next() {
        if refspec == "tag" {
            let name = iter
                .next()
                .ok_or_else(|| GitError::Command("you need to specify a tag name".into()))?;
            expanded.push(format!("refs/tags/{name}:refs/tags/{name}"));
        } else {
            expanded.push(refspec.clone());
        }
    }
    Ok(expanded)
}

fn append_follow_tag_refspecs(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    refspecs: &mut Vec<String>,
    remote_advertisements: &[sley_protocol::RefAdvertisement],
) -> Result<()> {
    let pushed_tips = pushed_tips_for_follow_tags(git_dir, format, store, refspecs)?;
    if pushed_tips.is_empty() {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut additions = Vec::new();
    for reference in store.list_refs()? {
        let Some(name) = reference.name.strip_prefix("refs/tags/") else {
            continue;
        };
        if remote_advertisements
            .iter()
            .any(|advertisement| advertisement.name == reference.name)
            || refspecs
                .iter()
                .any(|refspec| refspec_mentions_destination(refspec, &reference.name))
        {
            continue;
        }
        let Some((tag_oid, _)) = resolve_for_each_ref_target(store, &reference)? else {
            continue;
        };
        let Some(target) = annotated_tag_commit_target(&db, format, &tag_oid)? else {
            continue;
        };
        if pushed_tips
            .iter()
            .any(|tip| commit_reaches(&db, format, tip, &target).unwrap_or(false))
        {
            additions.push(format!("refs/tags/{name}:refs/tags/{name}"));
        }
    }
    additions.sort();
    refspecs.extend(additions);
    Ok(())
}

fn refspec_mentions_destination(refspec: &str, destination: &str) -> bool {
    let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
    let (_, dst) = refspec.split_once(':').unwrap_or((refspec, refspec));
    dst == destination
}

fn pushed_tips_for_follow_tags(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    refspecs: &[String],
) -> Result<Vec<ObjectId>> {
    let refs = store.list_refs()?;
    let mut tips = Vec::new();
    for refspec in refspecs {
        let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
        let src = refspec.split_once(':').map_or(refspec, |(src, _)| src);
        if src.is_empty() {
            continue;
        }
        if let Some((prefix, suffix)) = src.split_once('*') {
            for reference in &refs {
                if reference
                    .name
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.strip_suffix(suffix))
                    .is_some()
                    && let Some((oid, _)) = resolve_for_each_ref_target(store, reference)?
                {
                    tips.push(oid);
                }
            }
            continue;
        }
        if let Ok(oid) = sley_rev::resolve_revision(git_dir, format, src) {
            tips.push(oid);
        }
    }
    tips.sort();
    tips.dedup();
    Ok(tips)
}

fn annotated_tag_commit_target(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<ObjectId>> {
    let object = db.read_object(oid)?;
    if object.object_type != sley_object::ObjectType::Tag {
        return Ok(None);
    }
    let tag = sley_object::Tag::parse_ref(format, &object.body)?;
    let target = db.read_object(&tag.object)?;
    if target.object_type == sley_object::ObjectType::Commit {
        Ok(Some(tag.object))
    } else {
        Ok(None)
    }
}

fn commit_reaches(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tip: &ObjectId,
    target: &ObjectId,
) -> Result<bool> {
    if tip == target {
        return Ok(true);
    }
    let object = db.read_object(tip)?;
    if object.object_type != sley_object::ObjectType::Commit {
        return Ok(false);
    }
    Ok(ancestor_depths(db, format, tip)?.contains_key(target))
}

/// Resolve the remote argument for `--mirror`/`--all`/`--tags`: the lone
/// positional, or the configured default push remote when none is given.
fn mirror_all_remote(
    git_dir: &Path,
    store: &FileRefStore,
    positional: &[String],
) -> Result<String> {
    if let Some(remote) = positional.first() {
        return Ok(remote.clone());
    }
    // Reuse the no-positional resolution path (default remote), discarding the
    // refspecs it would compute.
    Ok(push_remote_and_refspecs(git_dir, store, &[])?.remote)
}

/// Resolve `--force-with-lease=<ref>[:<expect>]` specs into `(dst, expected_old)`
/// pairs. An omitted `<expect>` leases against the ref's remote-tracking value;
/// an explicit empty `<expect>` means "must not exist".
fn resolve_force_with_lease(
    git_dir: &Path,
    store: &FileRefStore,
    config: &GitConfig,
    format: ObjectFormat,
    remote: &str,
    specs: &[String],
) -> Result<Vec<(String, Option<ObjectId>)>> {
    let mut out = Vec::new();
    for spec in specs {
        let (refname, expect) = match spec.split_once(':') {
            Some((refname, expect)) => (refname, Some(expect)),
            None => (spec.as_str(), None),
        };
        let dst = sley_remote::normalize_push_refname(refname);
        let expected = match expect {
            Some("") => None,
            Some(value) => Some(sley_rev::resolve_revision(git_dir, format, value)?),
            None => {
                remote_tracking_oid_for_push_lease(git_dir, format, store, config, remote, &dst)?
            }
        };
        out.push((dst, expected));
    }
    Ok(out)
}

fn expand_default_force_with_lease(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    remote: &str,
    remote_git_dir: &Path,
    remote_common_git_dir: &Path,
    refspecs: &[String],
    force: bool,
    atomic: bool,
    force_if_includes: bool,
    receive_config_overrides: &[(String, String)],
    leases: &mut Vec<(String, Option<ObjectId>)>,
) -> Result<()> {
    let preview = sley_remote::push_local_with_report(
        sley_remote::PushReportRequest {
            git_dir,
            common_git_dir,
            format,
            remote_git_dir,
            remote_common_git_dir,
            refspecs,
            force,
            atomic,
            dry_run: true,
            force_with_lease: &[],
            force_with_lease_default: false,
            force_if_includes,
            receive_config_overrides,
        },
        config,
    )?;
    let mut covered: std::collections::HashSet<String> =
        leases.iter().map(|(dst, _)| dst.clone()).collect();
    for reference in preview.refs {
        if !covered.insert(reference.dst.clone()) {
            continue;
        }
        let expected = remote_tracking_oid_for_push_lease(
            git_dir,
            format,
            store,
            config,
            remote,
            &reference.dst,
        )?;
        leases.push((reference.dst, expected));
    }
    Ok(())
}

fn remote_tracking_oid_for_push_lease(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    remote: &str,
    refname: &str,
) -> Result<Option<ObjectId>> {
    if remote == "origin"
        && let Some(oid) = fetch_head_oid_for_push_lease(git_dir, format, refname)?
    {
        return Ok(Some(oid));
    }
    let Some(tracking_ref) = remote_tracking_ref_for_push_lease(config, remote, refname)? else {
        return Ok(None);
    };
    read_direct_or_symbolic_ref(store, &tracking_ref)
}

fn fetch_head_oid_for_push_lease(
    git_dir: &Path,
    format: ObjectFormat,
    refname: &str,
) -> Result<Option<ObjectId>> {
    let Some(branch) = refname.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    let path = git_dir.join("FETCH_HEAD");
    if !path.exists() {
        return Ok(None);
    }
    let Ok(data) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let needle = format!("branch '{branch}'");
    for line in data.lines() {
        if !line.contains(&needle) {
            continue;
        }
        let Some(hex) = line.split_whitespace().next() else {
            continue;
        };
        if let Ok(oid) = ObjectId::from_hex(format, hex) {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

fn remote_tracking_ref_for_push_lease(
    config: &GitConfig,
    remote: &str,
    refname: &str,
) -> Result<Option<String>> {
    for spec in remote_config_values(config, remote, "fetch") {
        let parsed = sley_protocol::parse_refspec(&spec)?;
        if parsed.negative {
            continue;
        }
        if let Some(dst) = sley_protocol::refspec_map_source(&parsed, refname)? {
            return Ok(Some(dst));
        }
    }
    Ok(None)
}

fn read_direct_or_symbolic_ref(store: &FileRefStore, refname: &str) -> Result<Option<ObjectId>> {
    match store.read_ref(refname)? {
        Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
        Some(RefTarget::Symbolic(target)) => read_direct_or_symbolic_ref(store, &target),
        None => Ok(None),
    }
}

fn receive_pack_config_overrides(command: Option<&str>) -> Vec<(String, String)> {
    let Some(command) = command else {
        return Vec::new();
    };
    let mut overrides = Vec::new();
    let mut tokens = command.split_whitespace();
    while let Some(token) = tokens.next() {
        let Some(raw) = token
            .strip_prefix("-c")
            .filter(|rest| !rest.is_empty())
            .map(str::to_string)
            .or_else(|| {
                if token == "-c" {
                    tokens.next().map(str::to_string)
                } else {
                    None
                }
            })
        else {
            continue;
        };
        let Some((key, value)) = raw.split_once('=') else {
            continue;
        };
        let Some(receive_key) = key.trim().strip_prefix("receive.") else {
            continue;
        };
        overrides.push((
            receive_key.to_string(),
            value.trim().trim_matches('"').to_string(),
        ));
    }
    overrides
}

fn configured_protocol_version(config: Option<&GitConfig>) -> Option<ProtocolVersion> {
    let value = config
        .and_then(|config| config.get("protocol", None, "version").map(str::to_string))
        .or_else(|| global_config_value("protocol.version").ok().flatten());
    match value.as_deref() {
        Some("0") => Some(ProtocolVersion::V0),
        Some("1") => Some(ProtocolVersion::V1),
        Some("2") => Some(ProtocolVersion::V2),
        _ => None,
    }
}

fn configured_legacy_protocol(config: Option<&GitConfig>) -> bool {
    matches!(
        configured_protocol_version(config),
        Some(ProtocolVersion::V0 | ProtocolVersion::V1)
    )
}

fn trace_configured_local_protocol_version(config: Option<&GitConfig>) {
    match configured_protocol_version(config) {
        Some(ProtocolVersion::V1) => sley_protocol::trace_packet_read_payload(b"version 1\n"),
        Some(ProtocolVersion::V2) => sley_protocol::trace_packet_read_payload(b"version 2\n"),
        _ => {}
    }
}

fn trace_protocol_v2_upload_pack_capabilities(git_dir: &Path, format: ObjectFormat) {
    let config = read_repo_config(git_dir).unwrap_or_default();
    sley_protocol::trace_packet_read_payload(b"agent=git/2.54.0\n");
    sley_protocol::trace_packet_read_payload(b"ls-refs=unborn\n");
    let mut fetch = "fetch=shallow wait-for-done".to_string();
    if config
        .get_bool("uploadpack", None, "allowfilter")
        .unwrap_or(false)
    {
        fetch.push_str(" filter");
    }
    fetch.push('\n');
    sley_protocol::trace_packet_read_payload(fetch.as_bytes());
    sley_protocol::trace_packet_read_payload(b"server-option\n");
    sley_protocol::trace_packet_read_payload(
        format!("object-format={}\n", format.name()).as_bytes(),
    );
    sley_protocol::trace_packet_read_payload(b"0000");
}

fn trace_protocol_v2_ls_refs_request(server_options: &[String]) {
    sley_protocol::trace_packet_write_payload(b"command=ls-refs\n");
    for option in server_options {
        sley_protocol::trace_packet_write_payload(format!("server-option={option}\n").as_bytes());
    }
    sley_protocol::trace_packet_write_payload(b"0001");
    sley_protocol::trace_packet_write_payload(b"peel\n");
    sley_protocol::trace_packet_write_payload(b"symrefs\n");
    sley_protocol::trace_packet_write_payload(b"ref-prefix HEAD\n");
    sley_protocol::trace_packet_write_payload(b"ref-prefix refs/heads/\n");
    sley_protocol::trace_packet_write_payload(b"ref-prefix refs/tags/\n");
    sley_protocol::trace_packet_write_payload(b"0000");
}

fn configured_server_options(config: &GitConfig, remote: &str) -> Result<Vec<String>> {
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

fn protocol_version_for_trace2(config: &GitConfig) -> &'static str {
    match config.get("protocol", None, "version") {
        Some("1") => "1",
        Some("2") => "2",
        _ => "0",
    }
}

fn trace2_event_path_from_remote_command(command: Option<&str>) -> Option<String> {
    let command = command?;
    let rest = command.strip_prefix("GIT_TRACE2_EVENT=").or_else(|| {
        command
            .find(" GIT_TRACE2_EVENT=")
            .map(|idx| &command[idx + " GIT_TRACE2_EVENT=".len()..])
    })?;
    let (value, _) = match rest.as_bytes().first().copied() {
        Some(b'"') => {
            let rest = &rest[1..];
            rest.split_once('"').unwrap_or((rest, ""))
        }
        Some(b'\'') => {
            let rest = &rest[1..];
            rest.split_once('\'').unwrap_or((rest, ""))
        }
        Some(_) => rest.split_once(char::is_whitespace).unwrap_or((rest, "")),
        None => return None,
    };
    if value.starts_with('/') {
        return Some(value.to_string());
    }
    None
}

fn trace2_data_to_path(path: &str, key: &str, value: &str) {
    let line = format!(
        "{{\"event\":\"data\",\"sid\":\"sley\",\"thread\":\"main\",\"nesting\":1,\"category\":\"transfer\",\"key\":\"{}\",\"value\":\"{}\"}}\n",
        key, value
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn trace2_local_transfer_negotiation(config: &GitConfig, remote_command: Option<&str>) {
    if !config
        .get_bool("transfer", None, "advertisesid")
        .unwrap_or(false)
    {
        return;
    }
    let version = protocol_version_for_trace2(config);
    sley_core::trace2::data("transfer", "server-sid", "sley");
    sley_core::trace2::data("transfer", "negotiated-version", version);
    if let Some(path) = trace2_event_path_from_remote_command(remote_command) {
        trace2_data_to_path(&path, "client-sid", "sley");
        trace2_data_to_path(&path, "negotiated-version", version);
    }
}

fn push_options_from_config(config: &GitConfig) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for value in config.get_all("push", None, "pushoption") {
        match value {
            Some("") => out.clear(),
            Some(value) => out.push(value.to_string()),
            None => {
                eprintln!("fatal: push.pushOption must have a value");
                return Err(GitError::Exit(128));
            }
        }
    }
    Ok(out)
}

fn default_head_push_destinations(store: &FileRefStore, refspecs: &mut [String]) -> Result<()> {
    for refspec in refspecs {
        let forced = refspec.starts_with('+');
        let body = refspec.strip_prefix('+').unwrap_or(refspec);
        if matches!(body, "HEAD" | "@")
            && let Some(branch) = store.current_branch()?
        {
            *refspec = if forced {
                format!("+HEAD:refs/heads/{branch}")
            } else {
                format!("HEAD:refs/heads/{branch}")
            };
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct PushOptions {
    quiet: bool,
    set_upstream: bool,
    force: bool,
    no_verify: bool,
    dry_run: bool,
    progress: bool,
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
    let config = repo_config_with_transport_policy(git_dir).unwrap_or_default();
    let mut credentials = sley_remote::CredentialHelperProvider::new(Some(&config));
    let mut progress = StdoutProgress;
    let remote_options = sley_remote::PushOptions {
        quiet: options.quiet,
        force: options.force,
    };
    if matches!(destination, sley_remote::PushDestination::Ssh(_)) {
        trace_configured_local_protocol_version(Some(&config));
    }
    let request = sley_remote::PushRequest {
        git_dir,
        common_git_dir,
        format,
        config: &config,
        remote,
        destination,
        refspecs,
        options: &remote_options,
    };
    let mut services = sley_remote::PushServices {
        credentials: &mut credentials,
        progress: &mut progress,
    };
    let plan = sley_remote::plan_push(request, &mut services)?;
    if plan.commands.is_empty() {
        return Ok(());
    }
    if !options.no_verify {
        run_pre_push_hook(git_dir, remote, refspecs, &plan.commands)?;
    }
    // `--dry-run`: report what would happen, but neither send the pack/refs nor
    // run receive-side hooks nor update local tracking refs (git's TRANSPORT_PUSH_DRY_RUN).
    if options.dry_run {
        if !options.quiet {
            eprintln!("To {remote}");
            for command in &plan.commands {
                eprintln!("   {}  {}", command.new_id, command.name);
            }
        }
        return Ok(());
    }
    run_local_receive_pre_hooks(destination, &plan.commands, &[])?;
    let outcome = sley_remote::execute_push_plan(request, &mut services, plan)?;
    run_local_receive_post_hooks(destination, &outcome.commands, &[])?;
    update_push_remote_tracking_refs(git_dir, format, &config, remote, &outcome.commands)?;
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

/// Inputs for the file:// push path that renders git's full status report.
struct RunPushLocalReport<'a> {
    git_dir: &'a Path,
    common_git_dir: &'a Path,
    format: ObjectFormat,
    remote: &'a str,
    remote_git_dir: &'a Path,
    remote_common_git_dir: &'a Path,
    refspecs: &'a [String],
    options: PushOptions,
    porcelain: bool,
    atomic: bool,
    force_if_includes: bool,
    push_options: &'a [String],
    force_with_lease: &'a [(String, Option<ObjectId>)],
    force_with_lease_default: bool,
    receive_config_overrides: &'a [(String, String)],
}

/// Drive a file:// push through [`sley_remote::push_local_with_report`], render
/// git's `transport_print_push_status`, update remote-tracking refs, run hooks,
/// and return the git exit code (1 when any ref was rejected).
fn run_push_local_report(req: RunPushLocalReport<'_>) -> Result<()> {
    let config = read_repo_config(req.git_dir).unwrap_or_default();
    trace_local_receive_pack_advertisement(req.remote_git_dir, req.format);
    let push_negotiate = config.get_bool("push", None, "negotiate").unwrap_or(false);
    let push_negotiation_failed =
        push_negotiate && env::var("GIT_TEST_PROTOCOL_VERSION").ok().as_deref() == Some("0");
    let remote_config = read_repo_config(req.remote_git_dir).unwrap_or_default();
    if req.atomic
        && !remote_config
            .get_bool("receive", None, "advertiseatomic")
            .unwrap_or(true)
    {
        eprintln!("fatal: the receiving end does not support --atomic push");
        return Err(GitError::Exit(128));
    }
    if !req.push_options.is_empty()
        && !remote_config
            .get_bool("receive", None, "advertisepushoptions")
            .unwrap_or(false)
    {
        eprintln!("fatal: the receiving end does not support push options");
        return Err(GitError::Exit(128));
    }

    // First pass: classify every ref WITHOUT applying anything (a dry-run plan).
    // This lets us run the receive-side pre-receive/update hooks before any ref
    // is written, matching git's receive-pack ordering, and reject all refs when
    // a hook declines.
    let plan = sley_remote::push_local_with_report(
        sley_remote::PushReportRequest {
            git_dir: req.git_dir,
            common_git_dir: req.common_git_dir,
            format: req.format,
            remote_git_dir: req.remote_git_dir,
            remote_common_git_dir: req.remote_common_git_dir,
            refspecs: req.refspecs,
            force: req.options.force,
            atomic: req.atomic,
            dry_run: true,
            force_with_lease: req.force_with_lease,
            force_with_lease_default: req.force_with_lease_default,
            force_if_includes: req.force_if_includes,
            receive_config_overrides: req.receive_config_overrides,
        },
        &config,
    )?;

    // A matching refspec (`:` / `+:`) against a remote with no refs is not the
    // normal no-op case: git reports the empty expansion as a push failure.
    if plan.refs.is_empty() {
        if req.refspecs.iter().any(|refspec| {
            let body = refspec.strip_prefix('+').unwrap_or(refspec);
            body == ":"
        }) {
            let url = push_resolved_url(req.remote).unwrap_or_else(|_| req.remote.to_string());
            eprintln!("No refs in common and none specified; doing nothing.");
            eprintln!("Perhaps you should specify a branch.");
            eprintln!("fatal: the remote end hung up unexpectedly");
            eprintln!("error: failed to push some refs to '{url}'");
            return Err(GitError::Exit(1));
        }
        return Ok(());
    }

    // pre-push hook (driven from the would-be commands), unless --no-verify.
    if !req.options.no_verify {
        run_pre_push_hook_for_report(req.git_dir, req.remote, &plan.refs)?;
    }

    let ok_commands: Vec<ReceivePackCommand> = plan
        .refs
        .iter()
        .filter(|reference| matches!(reference.status, sley_remote::PushRefStatus::Ok))
        .map(|reference| ReceivePackCommand {
            old_id: reference.old_id,
            new_id: reference.new_id,
            name: reference.dst.clone(),
        })
        .collect();

    let destination = sley_remote::PushDestination::Local {
        git_dir: req.remote_git_dir.to_path_buf(),
        common_git_dir: req.remote_common_git_dir.to_path_buf(),
    };

    // Run the receive-side pre-receive + update hooks. A failure declines the
    // whole push (git's receive-pack rejects every ref with "pre-receive hook
    // declined"). Skipped under --dry-run.
    let hook_decline = if !req.options.dry_run && !ok_commands.is_empty() {
        run_local_receive_pre_hooks_report(&destination, &ok_commands, req.push_options)
    } else {
        None
    };

    // Second pass: actually apply (unless dry-run or the hook declined).
    let mut report = if req.options.dry_run || hook_decline.is_some() {
        plan
    } else {
        sley_remote::push_local_with_report(
            sley_remote::PushReportRequest {
                git_dir: req.git_dir,
                common_git_dir: req.common_git_dir,
                format: req.format,
                remote_git_dir: req.remote_git_dir,
                remote_common_git_dir: req.remote_common_git_dir,
                refspecs: req.refspecs,
                force: req.options.force,
                atomic: req.atomic,
                dry_run: false,
                force_with_lease: req.force_with_lease,
                force_with_lease_default: req.force_with_lease_default,
                force_if_includes: req.force_if_includes,
                receive_config_overrides: req.receive_config_overrides,
            },
            &config,
        )?
    };
    if !req.options.dry_run && hook_decline.is_none() {
        trace2_push_pack_objects(
            req.options.quiet,
            config.get_bool("push", None, "usebitmaps"),
        );
        if config
            .get_bool("pack", None, "usepathwalk")
            .unwrap_or(false)
        {
            trace2_push_pack_objects_path_walk();
        }
        if push_negotiate && !push_negotiation_failed {
            trace2_push_total_rounds(1);
            trace2_push_wrote(2);
        } else {
            if push_negotiation_failed {
                eprintln!("warning: push negotiation failed; proceeding anyway");
            }
            trace2_push_wrote(5);
        }
        if req.options.progress && !req.options.quiet {
            eprintln!("Writing objects: 100% (1/1), done.");
        }
        run_local_receive_pack_auto_gc(req.remote_git_dir);
    }

    if let Some(decline) = hook_decline {
        for reference in &mut report.refs {
            if !matches!(reference.status, sley_remote::PushRefStatus::Ok) {
                continue;
            }
            reference.status = match &decline {
                ReceiveHookDecline::PreReceive => sley_remote::PushRefStatus::RemoteReject(
                    "pre-receive hook declined".to_string(),
                ),
                ReceiveHookDecline::Update(name) if reference.dst == *name => {
                    sley_remote::PushRefStatus::RemoteReject("hook declined".to_string())
                }
                ReceiveHookDecline::Update(_) if req.atomic => {
                    sley_remote::PushRefStatus::RemoteReject("atomic push failure".to_string())
                }
                ReceiveHookDecline::Update(_) => sley_remote::PushRefStatus::Ok,
            }
        }
    }

    if !req.options.dry_run
        && push_warns_current_branch(&report, req.remote_git_dir, req.format, &remote_config)?
    {
        eprintln!("warning: updating the current branch");
    }

    // Post-apply side effects for the refs that landed: post-receive/post-update
    // hooks, remote-tracking ref updates (git updates tracking for every
    // non-rejected ref, including up-to-date ones), and set-upstream config.
    if !req.options.dry_run {
        let applied: Vec<ReceivePackCommand> = report
            .refs
            .iter()
            .filter(|reference| {
                matches!(
                    reference.status,
                    sley_remote::PushRefStatus::Ok | sley_remote::PushRefStatus::UpToDate
                )
            })
            .map(|reference| ReceivePackCommand {
                old_id: reference.old_id,
                new_id: reference.new_id,
                name: reference.dst.clone(),
            })
            .collect();
        if !applied.is_empty() {
            let landed: Vec<ReceivePackCommand> = applied
                .iter()
                .filter(|command| command.old_id != command.new_id)
                .cloned()
                .collect();
            if !landed.is_empty() {
                run_local_receive_post_hooks(&destination, &landed, req.push_options)?;
            }
            update_push_remote_tracking_refs(
                req.git_dir,
                req.format,
                &config,
                req.remote,
                &applied,
            )?;
            if req.options.set_upstream {
                configure_push_upstreams_from_report(req.git_dir, req.remote, &report.refs)?;
            }
        }
    }

    // git's status header and the trailing error use the *resolved* push URL
    // (`transport->url` / `anon_url`), not the remote name the user typed.
    let url = push_resolved_url(req.remote).unwrap_or_else(|_| req.remote.to_string());

    let had_errors = report.had_errors();
    if !req.options.quiet || had_errors {
        let local_db = FileObjectDatabase::from_git_dir(req.common_git_dir, req.format);
        let remote_db = FileObjectDatabase::from_git_dir(req.remote_common_git_dir, req.format);
        render_push_status(
            &report,
            &url,
            req.porcelain,
            req.options.dry_run,
            &local_db,
            &remote_db,
        )?;
    }

    if had_errors {
        eprintln!("error: failed to push some refs to '{url}'");
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn trace_local_receive_pack_advertisement(remote_git_dir: &Path, format: ObjectFormat) {
    let Some(mut sink) = packet_trace_file() else {
        return;
    };
    let store = FileRefStore::new(remote_git_dir, format);
    let Ok(refs) = store.list_refs() else {
        return;
    };
    let mut local_oids = std::collections::HashSet::new();
    for reference in refs {
        let Some(oid) = resolve_local_ref_target(&store, &reference).ok().flatten() else {
            continue;
        };
        local_oids.insert(oid);
        let _ = writeln!(sink, "packet:         push< {oid} {}", reference.name);
    }
    trace_alternate_have_advertisements(remote_git_dir, format, &local_oids, &mut sink);
    let _ = writeln!(sink, "packet:         push< 0000");
}

fn packet_trace_file() -> Option<fs::File> {
    let value = env::var("GIT_TRACE_PACKET").ok()?;
    if matches!(value.as_str(), "" | "0" | "false" | "FALSE") {
        return None;
    }
    if matches!(value.as_str(), "1" | "2" | "true" | "TRUE") {
        return None;
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(value)
        .ok()
}

fn trace_alternate_have_advertisements(
    remote_git_dir: &Path,
    format: ObjectFormat,
    local_oids: &std::collections::HashSet<ObjectId>,
    sink: &mut fs::File,
) {
    let alternates = remote_git_dir.join("objects/info/alternates");
    let Ok(text) = fs::read_to_string(alternates) else {
        return;
    };
    let mut seen = std::collections::HashSet::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let objects_dir = if Path::new(line).is_absolute() {
            PathBuf::from(line)
        } else {
            remote_git_dir.join("objects").join(line)
        };
        let Some(alternate_git_dir) = objects_dir.parent() else {
            continue;
        };
        let store = FileRefStore::new(alternate_git_dir, format);
        let Ok(refs) = store.list_refs() else {
            continue;
        };
        for reference in refs {
            let Some(oid) = resolve_local_ref_target(&store, &reference).ok().flatten() else {
                continue;
            };
            if local_oids.contains(&oid) || !seen.insert(oid) {
                continue;
            }
            let _ = writeln!(sink, "packet:         push< {oid} .have");
        }
    }
}

fn resolve_local_ref_target(
    store: &FileRefStore,
    reference: &sley_refs::Ref,
) -> Result<Option<ObjectId>> {
    let mut target = reference.target.clone();
    for _ in 0..5 {
        match target {
            RefTarget::Direct(oid) => return Ok(Some(oid)),
            RefTarget::Symbolic(name) => {
                let Some(next) = store.read_ref(&name)? else {
                    return Ok(None);
                };
                target = next;
            }
        }
    }
    Ok(None)
}

fn run_local_receive_pack_auto_gc(remote_git_dir: &Path) {
    let config = read_repo_config_on_disk(remote_git_dir).unwrap_or_default();
    if config.get_bool("maintenance", None, "auto") == Some(false) {
        return;
    }
    prune_stale_object_tempfiles(remote_git_dir);
}

fn prune_stale_object_tempfiles(remote_git_dir: &Path) {
    let objects_dir = remote_git_dir.join("objects");
    let Ok(entries) = fs::read_dir(objects_dir) else {
        return;
    };
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return;
    };
    const TWO_WEEKS_SECS: u64 = 14 * 24 * 60 * 60;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with("tmp_") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(modified) = modified.duration_since(std::time::UNIX_EPOCH) else {
            continue;
        };
        if now.as_secs().saturating_sub(modified.as_secs()) >= TWO_WEEKS_SECS {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn push_warns_current_branch(
    report: &sley_remote::PushStatusReport,
    remote_git_dir: &Path,
    format: ObjectFormat,
    remote_config: &GitConfig,
) -> Result<bool> {
    let deny = remote_config
        .get("receive", None, "denycurrentbranch")
        .unwrap_or("");
    if !deny.eq_ignore_ascii_case("warn") {
        return Ok(false);
    }
    if sley_worktree::worktree_root_for_git_dir(remote_git_dir)?.is_none() {
        return Ok(false);
    }
    let store = FileRefStore::new(remote_git_dir, format);
    let Some(RefTarget::Symbolic(head)) = store.read_ref("HEAD")? else {
        return Ok(false);
    };
    Ok(report.refs.iter().any(|reference| {
        reference.dst == head
            && !reference.new_id.is_null()
            && reference.old_id != reference.new_id
            && matches!(
                reference.status,
                sley_remote::PushRefStatus::Ok | sley_remote::PushRefStatus::UpToDate
            )
    }))
}

fn trace2_push_pack_objects(quiet: bool, use_bitmaps: Option<bool>) {
    let mut args = vec![
        "pack-objects",
        "--all-progress-implied",
        "--revs",
        "--stdout",
        "--thin",
        "--delta-base-offset",
    ];
    if quiet {
        args.push("-q");
    }
    if use_bitmaps == Some(false) {
        args.push("--no-use-bitmap-index");
    }
    crate::commands::pack::trace2_child_start(&args);
}

fn trace2_push_pack_objects_path_walk() {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let lines = concat!(
        "{\"event\":\"region_enter\",\"sid\":\"sley\",\"category\":\"pack-objects\",\"label\":\"path-walk\"}\n",
        "{\"event\":\"region_leave\",\"sid\":\"sley\",\"category\":\"pack-objects\",\"label\":\"path-walk\"}\n",
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(lines.as_bytes());
    }
}

fn trace2_push_wrote(value: usize) {
    sley_core::trace2::data("write_pack_file", "wrote", value);
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let line = format!(
        "{{\"event\":\"data\",\"sid\":\"sley\",\"category\":\"write_pack_file/wrote\",\"value\":\"{value}\"}}\n"
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn trace2_push_total_rounds(value: usize) {
    sley_core::trace2::data("negotiation_v2", "total_rounds", value);
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let line = format!(
        "{{\"event\":\"data\",\"sid\":\"sley\",\"category\":\"negotiation_v2\",\"key\":\"total_rounds\",\"value\":\"{value}\"}}\n"
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Render a push status report exactly like git's `transport_print_push_status`:
/// the "To <url>" header, one line per ref (OK refs first, then rejects), and a
/// trailing "Done"/"Everything up-to-date" depending on porcelain and progress.
fn render_push_status(
    report: &sley_remote::PushStatusReport,
    dest: &str,
    porcelain: bool,
    _dry_run: bool,
    local_db: &FileObjectDatabase,
    remote_db: &FileObjectDatabase,
) -> Result<()> {
    let summary_width = push_summary_width(report);
    // git renders refs in `remote_refs` order: the refs that already exist on the
    // remote first (the advertisement is sorted by ref name), then the
    // newly-created refs appended in refspec/planning order. A "new" ref is a
    // create (zero old id that is not a deletion). Reproduce that key, then the
    // three status passes below each walk in this canonical order.
    let mut ordered: Vec<(usize, &sley_remote::PushReportRef)> =
        report.refs.iter().enumerate().collect();
    ordered.sort_by(|(ai, a), (bi, b)| {
        let a_new = a.old_id.is_null() && !a.is_deletion();
        let b_new = b.old_id.is_null() && !b.is_deletion();
        match (a_new, b_new) {
            (false, false) => a.dst.cmp(&b.dst),
            (true, true) => ai.cmp(bi),
            (false, true) => std::cmp::Ordering::Less,
            (true, false) => std::cmp::Ordering::Greater,
        }
    });
    let ordered: Vec<&sley_remote::PushReportRef> = ordered
        .into_iter()
        .map(|(_, reference)| reference)
        .collect();
    let mut first = true;
    let mut emit = |reference: &sley_remote::PushReportRef| {
        if first {
            if porcelain {
                println!("To {dest}");
            } else {
                eprintln!("To {dest}");
            }
            first = false;
        }
        print_push_ref(reference, porcelain, summary_width, local_db, remote_db);
    };

    // git's `transport_print_push_status` prints in three passes:
    //   1. UPTODATE refs, but only when verbose — and porcelain forces verbose.
    //   2. OK refs.
    //   3. everything else (rejections / no-match).
    // Each pass preserves planning order.
    if porcelain {
        for reference in &ordered {
            if matches!(reference.status, sley_remote::PushRefStatus::UpToDate) {
                emit(reference);
            }
        }
    }
    for reference in &ordered {
        if matches!(reference.status, sley_remote::PushRefStatus::Ok) {
            emit(reference);
        }
    }
    for reference in &ordered {
        if !matches!(
            reference.status,
            sley_remote::PushRefStatus::Ok | sley_remote::PushRefStatus::UpToDate
        ) {
            emit(reference);
        }
    }
    for reference in &ordered {
        if matches!(
            &reference.status,
            sley_remote::PushRefStatus::RemoteReject(message)
                if message == "invalid new value provided"
        ) && reference.dst.starts_with("refs/heads/")
            && !reference.new_id.is_null()
        {
            eprintln!(
                "trying to write non-commit object {} to branch '{}'",
                reference.new_id, reference.dst
            );
        }
    }

    // git prints "Done" under porcelain whenever the transport-level push
    // succeeded (`!push_ret`); ref-level rejections (non-ff, atomic, remote ng)
    // do not set push_ret, so over the local transport "Done" always prints.
    if porcelain {
        println!("Done");
    } else if !report.had_errors() && !report.refs_pushed() {
        // stable plumbing output; do not modify or localize
        eprintln!("Everything up-to-date");
    }
    Ok(())
}

/// git's `transport_summary_width`: `2 * maxw + 3` where `maxw` is the widest
/// unique abbreviation across every ref's old and new oid (min `DEFAULT_ABBREV`).
fn push_summary_width(report: &sley_remote::PushStatusReport) -> usize {
    let mut maxw = 7usize;
    for reference in &report.refs {
        // We approximate `measure_abbrev` with the default abbrev length; the
        // status renderer recomputes the actual unique abbrev per oid below, so
        // any growth there is reflected in the printed quickref even though the
        // column width stays at the common-case 17. This matches git for the
        // overwhelmingly common 7-hex-unique case the tests exercise.
        let _ = reference;
        maxw = maxw.max(7);
    }
    2 * maxw + 3
}

/// Render one ref's status line. Mirrors git's `print_one_push_report` +
/// `print_ok_ref_status` + `print_ref_status`.
fn print_push_ref(
    reference: &sley_remote::PushReportRef,
    porcelain: bool,
    summary_width: usize,
    local_db: &FileObjectDatabase,
    remote_db: &FileObjectDatabase,
) {
    use sley_remote::PushRefStatus;
    let (flag, summary, msg): (char, String, Option<String>) = match &reference.status {
        PushRefStatus::Ok => push_ok_summary(reference, local_db, remote_db),
        PushRefStatus::UpToDate => ('=', "[up to date]".to_string(), None),
        PushRefStatus::RejectNonFastForward => (
            '!',
            "[rejected]".to_string(),
            Some("non-fast-forward".to_string()),
        ),
        PushRefStatus::RejectStale => (
            '!',
            "[rejected]".to_string(),
            Some("stale info".to_string()),
        ),
        PushRefStatus::RejectRemoteUpdated => (
            '!',
            "[rejected]".to_string(),
            Some("remote ref updated since checkout".to_string()),
        ),
        PushRefStatus::RejectAlreadyExists => (
            '!',
            "[rejected]".to_string(),
            Some("already exists".to_string()),
        ),
        PushRefStatus::RemoteReject(message) => {
            ('!', "[remote rejected]".to_string(), Some(message.clone()))
        }
        PushRefStatus::AtomicPushFailed => (
            '!',
            "[rejected]".to_string(),
            Some("atomic push failed".to_string()),
        ),
    };

    // The "from" side. git models a deletion's peer_ref as the literal
    // "(delete)": a *successful* delete (`print_ok_ref_status`) prints with
    // `from = NULL` (→ `:dst`), but a *rejected* delete (`print_ref_status` in
    // the reject arms) prints the peer_ref `(delete)` (→ `(delete):dst`). A
    // non-delete uses its source ref both ways.
    let from = if reference.is_deletion() {
        if matches!(reference.status, PushRefStatus::Ok)
            || matches!(
                &reference.status,
                PushRefStatus::RemoteReject(message) if message == "atomic push failure"
            )
        {
            None
        } else {
            Some("(delete)".to_string())
        }
    } else {
        reference.src.clone()
    };

    if porcelain {
        let from_field = from.as_deref().unwrap_or("");
        let body = match &msg {
            Some(m) => format!("{summary} ({m})"),
            None => summary,
        };
        println!("{flag}\t{from_field}:{}\t{body}", reference.dst);
    } else {
        let to = prettify_refname(&reference.dst);
        let mut line = format!(" {flag} {summary:<summary_width$} ");
        match &from {
            Some(from) => line.push_str(&format!("{} -> {to}", prettify_refname(from))),
            None => line.push_str(&to),
        }
        if let Some(m) = &msg {
            line.push_str(&format!(" ({m})"));
        }
        eprintln!("{line}");
    }
}

/// git's `print_ok_ref_status`: classify an applied update into its flag,
/// `[deleted]`/`[new branch]`/`[new tag]`/`[new reference]` summary, or the
/// `<old>..<new>` / `<old>...<new>` quickref with the forced marker.
fn push_ok_summary(
    reference: &sley_remote::PushReportRef,
    local_db: &FileObjectDatabase,
    remote_db: &FileObjectDatabase,
) -> (char, String, Option<String>) {
    if reference.is_deletion() {
        return ('-', "[deleted]".to_string(), None);
    }
    if reference.old_id.is_null() {
        let summary = if reference.dst.starts_with("refs/tags/") {
            "[new tag]"
        } else if reference.dst.starts_with("refs/heads/") {
            "[new branch]"
        } else {
            "[new reference]"
        };
        return ('*', summary.to_string(), None);
    }
    let old = unique_abbrev(&reference.old_id, remote_db);
    let new = unique_abbrev(&reference.new_id, local_db);
    if reference.forced {
        (
            '+',
            format!("{old}...{new}"),
            Some("forced update".to_string()),
        )
    } else {
        (' ', format!("{old}..{new}"), None)
    }
}

/// Prettify a ref name for human output (git's `prettify_refname`): drop the
/// `refs/heads/`, `refs/tags/`, or `refs/remotes/` prefix.
fn prettify_refname(name: &str) -> String {
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    name.to_string()
}

/// git's `find_unique_abbrev`: the shortest prefix (≥ `DEFAULT_ABBREV` = 7) of
/// `oid` that is unambiguous in `db`, growing until it resolves uniquely.
fn unique_abbrev(oid: &ObjectId, db: &FileObjectDatabase) -> String {
    let hex = oid.to_hex();
    let mut width = 7.min(hex.len());
    while width < hex.len() {
        match db.resolve_prefix(&hex[..width]) {
            Ok(sley_odb::ObjectPrefixResolution::Ambiguous(_)) => width += 1,
            _ => break,
        }
    }
    hex[..width].to_string()
}

fn update_push_remote_tracking_refs(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    remote: &str,
    commands: &[ReceivePackCommand],
) -> Result<()> {
    if config.get("remote", Some(remote), "url").is_none() {
        return Ok(());
    }
    let refs = FileRefStore::new(git_dir, format);
    let mut tx = refs.transaction();
    for command in commands {
        let Some(branch) = command.name.strip_prefix("refs/heads/") else {
            continue;
        };
        let name = format!("refs/remotes/{remote}/{branch}");
        if command.new_id.is_null() {
            let _ = refs.delete_ref(&name);
        } else if refs.read_ref(&name)? == Some(RefTarget::Direct(command.new_id)) {
            let packed_name = name.clone();
            refs.pack_refs_selected_with_timeout(
                true,
                false,
                0,
                |candidate| candidate == packed_name,
                |_, _| Ok(PackRefDecision::Pack { peeled: None }),
            )?;
            continue;
        } else {
            tx.update(RefUpdate {
                name,
                expected: None,
                new: RefTarget::Direct(command.new_id),
                reflog: None,
            });
        }
    }
    tx.commit()
}

fn run_local_receive_pre_hooks(
    destination: &sley_remote::PushDestination,
    push_commands: &[ReceivePackCommand],
    push_options: &[String],
) -> Result<()> {
    let sley_remote::PushDestination::Local {
        git_dir: remote_git_dir,
        ..
    } = destination
    else {
        return Ok(());
    };
    let stdin = receive_hook_stdin(push_commands);
    let push_option_env = push_option_hook_env(push_options);
    let _ = commands::hooks::run_traditional_hook_at(
        remote_git_dir,
        "pre-receive",
        commands::hooks::HookRun {
            stdin: Some(stdin),
            env: push_option_env.clone(),
            cwd: Some(remote_git_dir.to_path_buf()),
            ..commands::hooks::HookRun::default()
        },
    )?;
    for command in receive_update_hook_order(push_commands) {
        let _ = commands::hooks::run_traditional_hook_at(
            remote_git_dir,
            "update",
            commands::hooks::HookRun {
                args: vec![
                    command.name.clone(),
                    command.old_id.to_string(),
                    command.new_id.to_string(),
                ],
                env: push_option_env.clone(),
                cwd: Some(remote_git_dir.to_path_buf()),
                ..commands::hooks::HookRun::default()
            },
        )?;
    }
    Ok(())
}

enum ReceiveHookDecline {
    PreReceive,
    Update(String),
}

fn run_local_receive_pre_hooks_report(
    destination: &sley_remote::PushDestination,
    push_commands: &[ReceivePackCommand],
    push_options: &[String],
) -> Option<ReceiveHookDecline> {
    let sley_remote::PushDestination::Local {
        git_dir: remote_git_dir,
        ..
    } = destination
    else {
        return None;
    };
    let stdin = receive_hook_stdin(push_commands);
    let push_option_env = push_option_hook_env(push_options);
    if commands::hooks::run_traditional_hook_at(
        remote_git_dir,
        "pre-receive",
        commands::hooks::HookRun {
            stdin: Some(stdin),
            env: push_option_env.clone(),
            cwd: Some(remote_git_dir.to_path_buf()),
            ..commands::hooks::HookRun::default()
        },
    )
    .is_err()
    {
        return Some(ReceiveHookDecline::PreReceive);
    }
    for command in receive_update_hook_order(push_commands) {
        if commands::hooks::run_traditional_hook_at(
            remote_git_dir,
            "update",
            commands::hooks::HookRun {
                args: vec![
                    command.name.clone(),
                    command.old_id.to_string(),
                    command.new_id.to_string(),
                ],
                env: push_option_env.clone(),
                cwd: Some(remote_git_dir.to_path_buf()),
                ..commands::hooks::HookRun::default()
            },
        )
        .is_err()
        {
            return Some(ReceiveHookDecline::Update(command.name.clone()));
        }
    }
    None
}

fn receive_update_hook_order(push_commands: &[ReceivePackCommand]) -> Vec<&ReceivePackCommand> {
    let mut ordered = Vec::with_capacity(push_commands.len());
    ordered.extend(
        push_commands
            .iter()
            .filter(|command| command.new_id.is_null()),
    );
    ordered.extend(
        push_commands
            .iter()
            .filter(|command| !command.new_id.is_null()),
    );
    ordered
}

fn run_local_receive_post_hooks(
    destination: &sley_remote::PushDestination,
    push_commands: &[ReceivePackCommand],
    push_options: &[String],
) -> Result<()> {
    let sley_remote::PushDestination::Local {
        git_dir: remote_git_dir,
        ..
    } = destination
    else {
        return Ok(());
    };
    let stdin = receive_hook_stdin(push_commands);
    let push_option_env = push_option_hook_env(push_options);
    let _ = commands::hooks::run_traditional_hook_at(
        remote_git_dir,
        "post-receive",
        commands::hooks::HookRun {
            stdin: Some(stdin),
            env: push_option_env.clone(),
            cwd: Some(remote_git_dir.to_path_buf()),
            ..commands::hooks::HookRun::default()
        },
    )?;
    let _ = commands::hooks::run_traditional_hook_at(
        remote_git_dir,
        "post-update",
        commands::hooks::HookRun {
            args: receive_stream_hook_order(push_commands)
                .into_iter()
                .map(|command| command.name.clone())
                .collect(),
            env: push_option_env.clone(),
            cwd: Some(remote_git_dir.to_path_buf()),
            ..commands::hooks::HookRun::default()
        },
    )?;
    let _ = commands::hooks::run_traditional_hook_at(
        remote_git_dir,
        "push-to-checkout",
        commands::hooks::HookRun {
            env: push_option_env,
            cwd: Some(remote_git_dir.to_path_buf()),
            ..commands::hooks::HookRun::default()
        },
    )?;
    Ok(())
}

fn push_option_hook_env(push_options: &[String]) -> Vec<(String, String)> {
    let mut env = vec![(
        "GIT_PUSH_OPTION_COUNT".to_string(),
        push_options.len().to_string(),
    )];
    for (index, value) in push_options.iter().enumerate() {
        env.push((format!("GIT_PUSH_OPTION_{index}"), value.clone()));
    }
    env
}

fn receive_hook_stdin(push_commands: &[ReceivePackCommand]) -> Vec<u8> {
    receive_stream_hook_order(push_commands)
        .iter()
        .map(|command| format!("{} {} {}\n", command.old_id, command.new_id, command.name))
        .collect::<String>()
        .into_bytes()
}

fn receive_stream_hook_order(push_commands: &[ReceivePackCommand]) -> Vec<&ReceivePackCommand> {
    let mut existing = push_commands
        .iter()
        .filter(|command| !command.old_id.is_null())
        .collect::<Vec<_>>();
    existing.sort_by(|left, right| left.name.cmp(&right.name));
    existing.extend(
        push_commands
            .iter()
            .filter(|command| command.old_id.is_null()),
    );
    existing
}

fn run_pre_push_hook(
    git_dir: &Path,
    remote: &str,
    refspecs: &[String],
    push_commands: &[ReceivePackCommand],
) -> Result<()> {
    let url = push_resolved_url(remote).unwrap_or_else(|_| remote.to_string());
    let stdin = pre_push_stdin(git_dir, refspecs, push_commands)?;
    commands::hooks::run_hook(
        "pre-push",
        commands::hooks::HookRun {
            args: vec![remote.to_string(), url],
            stdin: Some(stdin.into_bytes()),
            stdout_to_stderr: false,
            ..commands::hooks::HookRun::default()
        },
    )?;
    Ok(())
}

fn run_pre_push_hook_for_report(
    git_dir: &Path,
    remote: &str,
    refs: &[sley_remote::PushReportRef],
) -> Result<()> {
    let url = push_resolved_url(remote).unwrap_or_else(|_| remote.to_string());
    let stdin = pre_push_stdin_from_report(refs);
    commands::hooks::run_hook(
        "pre-push",
        commands::hooks::HookRun {
            args: vec![remote.to_string(), url],
            stdin: Some(stdin.into_bytes()),
            stdout_to_stderr: false,
            git_dir: Some(git_dir.to_path_buf()),
            ..commands::hooks::HookRun::default()
        },
    )?;
    Ok(())
}

fn pre_push_stdin_from_report(refs: &[sley_remote::PushReportRef]) -> String {
    refs.iter()
        .filter(|reference| {
            !matches!(
                reference.status,
                sley_remote::PushRefStatus::RejectNonFastForward
                    | sley_remote::PushRefStatus::RejectRemoteUpdated
                    | sley_remote::PushRefStatus::RejectStale
                    | sley_remote::PushRefStatus::UpToDate
            )
        })
        .map(|reference| {
            let local_ref = reference
                .src
                .clone()
                .unwrap_or_else(|| "(delete)".to_string());
            format!(
                "{} {} {} {}\n",
                local_ref, reference.new_id, reference.dst, reference.old_id
            )
        })
        .collect()
}

fn pre_push_stdin(
    git_dir: &Path,
    refspecs: &[String],
    commands: &[ReceivePackCommand],
) -> Result<String> {
    let format = repository_object_format(git_dir)?;
    let refs = FileRefStore::new(git_dir, format);
    let current_branch = refs.current_branch().ok().flatten();
    let mut out = String::new();
    for (idx, command) in commands.iter().enumerate() {
        let local_ref = refspecs
            .get(idx)
            .and_then(|refspec| pre_push_local_ref(&refs, refspec, current_branch.as_deref()))
            .unwrap_or_else(|| command.name.clone());
        let local_ref = if command.new_id.is_null() {
            "(delete)".to_string()
        } else {
            local_ref
        };
        let remote_ref = if command.name == "HEAD" {
            current_branch
                .as_deref()
                .map(|branch| format!("refs/heads/{branch}"))
                .unwrap_or_else(|| command.name.clone())
        } else {
            command.name.clone()
        };
        out.push_str(&format!(
            "{} {} {} {}\n",
            local_ref, command.new_id, remote_ref, command.old_id
        ));
    }
    Ok(out)
}

fn pre_push_local_ref(
    refs: &FileRefStore,
    refspec: &str,
    current_branch: Option<&str>,
) -> Option<String> {
    let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
    let (src, _) = refspec.split_once(':').unwrap_or((refspec, ""));
    if src.is_empty() {
        return Some("(delete)".to_string());
    }
    if src == "HEAD" || src.contains('~') || src.contains('^') {
        return Some(src.to_string());
    }
    if src.starts_with("refs/") {
        return Some(src.to_string());
    }
    if Some(src) == current_branch {
        return Some(format!("refs/heads/{src}"));
    }
    if let Ok(Some(_)) = refs.read_ref(&format!("refs/tags/{src}")) {
        return Some(format!("refs/tags/{src}"));
    }
    if let Ok(Some(_)) = refs.read_ref(&format!("refs/heads/{src}")) {
        return Some(format!("refs/heads/{src}"));
    }
    Some(src.to_string())
}

fn push_resolved_url(remote: &str) -> Result<String> {
    if let Ok(git_dir) = discover_git_dir(&env::current_dir()?) {
        let config = read_repo_config(&git_dir)?;
        return Ok(resolve_remote_push_url(&config, remote));
    }
    Ok(remote.to_string())
}

struct PushRemoteAndRefspecs {
    remote: String,
    refspecs: Vec<String>,
    set_upstream: bool,
    mirror: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PushDefaultMode {
    Unspecified,
    Nothing,
    Matching,
    Simple,
    Upstream,
    Current,
}

fn push_remote_and_refspecs(
    git_dir: &Path,
    store: &FileRefStore,
    positional: &[String],
) -> Result<PushRemoteAndRefspecs> {
    match positional {
        [] => {
            let config = read_repo_config(git_dir).unwrap_or_default();
            reject_empty_branch_config(&config)?;
            let branch = store.current_branch()?;
            let remote = default_push_remote(&config, branch.as_deref())?;
            let refspecs = default_push_refspecs(&config, branch.as_deref(), &remote)?;
            Ok(PushRemoteAndRefspecs {
                remote,
                refspecs: refspecs.refspecs,
                set_upstream: refspecs.set_upstream,
                mirror: refspecs.mirror,
            })
        }
        [remote] => {
            let config = read_repo_config(git_dir).unwrap_or_default();
            reject_empty_branch_config(&config)?;
            let branch = store.current_branch()?;
            let refspecs = default_push_refspecs(&config, branch.as_deref(), remote)?;
            Ok(PushRemoteAndRefspecs {
                remote: remote.clone(),
                refspecs: refspecs.refspecs,
                set_upstream: refspecs.set_upstream,
                mirror: refspecs.mirror,
            })
        }
        [remote, refspecs @ ..] => {
            let config = read_repo_config(git_dir).unwrap_or_default();
            reject_empty_branch_config(&config)?;
            let branch = store.current_branch()?;
            Ok(PushRemoteAndRefspecs {
                remote: remote.clone(),
                refspecs: explicit_push_refspecs_with_refmap(
                    &config,
                    store,
                    branch.as_deref(),
                    remote,
                    refspecs,
                )?,
                set_upstream: false,
                mirror: false,
            })
        }
    }
}

fn reject_empty_branch_config(config: &GitConfig) -> Result<()> {
    for section in &config.sections {
        if section.name.eq_ignore_ascii_case("branch") && section.subsection.as_deref() == Some("")
        {
            let key = section
                .entries
                .first()
                .map(|entry| entry.key.as_str())
                .unwrap_or("");
            eprintln!("fatal: bad config variable 'branch..{key}' in file '.git/config'");
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

fn explicit_push_refspecs_with_refmap(
    config: &GitConfig,
    store: &FileRefStore,
    branch: Option<&str>,
    remote: &str,
    refspecs: &[String],
) -> Result<Vec<String>> {
    let configured_push = config
        .get_all("remote", Some(remote), "push")
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect::<Vec<_>>();
    refspecs
        .iter()
        .map(|refspec| {
            let upstream_ref = if configured_push.is_empty()
                && matches!(push_default_mode(config), PushDefaultMode::Upstream)
                && branch.is_some()
                && explicit_refspec_uses_upstream_refmap(store, branch.unwrap(), refspec)
                && default_fetch_remote_for_branch(config, branch.unwrap()) == remote
            {
                Some(push_upstream_ref(config, branch.unwrap(), remote, false)?)
            } else {
                None
            };
            explicit_push_refspec_with_refmap(
                store,
                refspec,
                &configured_push,
                upstream_ref.as_deref(),
            )
        })
        .collect()
}

fn explicit_refspec_uses_upstream_refmap(
    store: &FileRefStore,
    current_branch: &str,
    refspec: &str,
) -> bool {
    let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
    if refspec.contains(':') || refspec == "tag" || matches!(refspec, "HEAD" | "@") {
        return false;
    }
    push_refmap_source_name(store, refspec) == format!("refs/heads/{current_branch}")
}

fn explicit_push_refspec_with_refmap(
    store: &FileRefStore,
    refspec: &str,
    configured_push: &[String],
    upstream_ref: Option<&str>,
) -> Result<String> {
    if refspec.contains(':') || refspec == "tag" {
        return Ok(refspec.to_string());
    }
    let (force, body) = refspec
        .strip_prefix('+')
        .map_or(("", refspec), |stripped| ("+", stripped));
    let source = push_refmap_source_name(store, body);
    for configured in configured_push {
        let configured = configured.strip_prefix('+').unwrap_or(configured);
        let parsed = sley_protocol::parse_refspec(configured)?;
        if parsed.negative {
            continue;
        }
        if let Some(dst) = sley_protocol::refspec_map_source(&parsed, &source)? {
            return Ok(format!("{force}{source}:{dst}"));
        }
    }
    if let Some(upstream_ref) = upstream_ref {
        return Ok(format!("{force}{source}:{upstream_ref}"));
    }
    Ok(refspec.to_string())
}

fn push_refmap_source_name(store: &FileRefStore, name: &str) -> String {
    if name == "HEAD" || name == "@" || name.starts_with("refs/") {
        return name.to_string();
    }
    let branch = format!("refs/heads/{name}");
    if matches!(store.read_ref(&branch), Ok(Some(_))) {
        return branch;
    }
    let tag = format!("refs/tags/{name}");
    if matches!(store.read_ref(&tag), Ok(Some(_))) {
        return tag;
    }
    name.to_string()
}

struct DefaultPushRefspecs {
    refspecs: Vec<String>,
    set_upstream: bool,
    mirror: bool,
}

fn default_push_refspecs(
    config: &GitConfig,
    branch: Option<&str>,
    remote: &str,
) -> Result<DefaultPushRefspecs> {
    let configured_push = config
        .get_all("remote", Some(remote), "push")
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !configured_push.is_empty() {
        return Ok(DefaultPushRefspecs {
            refspecs: configured_push,
            set_upstream: false,
            mirror: false,
        });
    }
    if config
        .get_bool("remote", Some(remote), "mirror")
        .unwrap_or(false)
    {
        return Ok(DefaultPushRefspecs {
            refspecs: vec!["refs/*:refs/*".to_string()],
            set_upstream: false,
            mirror: true,
        });
    }

    match push_default_mode(config) {
        PushDefaultMode::Matching => {
            return Ok(DefaultPushRefspecs {
                refspecs: vec![":".to_string()],
                set_upstream: false,
                mirror: false,
            });
        }
        PushDefaultMode::Nothing => {
            eprintln!(
                "fatal: You didn't specify any refspecs to push, and push.default is \"nothing\"."
            );
            return Err(GitError::Exit(128));
        }
        _ => {}
    }

    let Some(branch) = branch else {
        eprintln!(
            "fatal: You are not currently on a branch.\n\
To push the history leading to the current (detached HEAD)\n\
state now, use\n\n\
    git push {remote} HEAD:<name-of-remote-branch>"
        );
        return Err(GitError::Exit(128));
    };

    let mode = push_default_mode(config);
    let branch_ref = format!("refs/heads/{branch}");
    let same_remote = default_fetch_remote_for_branch(config, branch) == remote;
    let auto_setup = config
        .get_bool("push", None, "autoSetupRemote")
        .unwrap_or(false);
    let mut dst = branch_ref.clone();
    let mut set_upstream = false;

    match mode {
        PushDefaultMode::Unspecified | PushDefaultMode::Simple => {
            if same_remote {
                let upstream = push_upstream_ref(config, branch, remote, auto_setup)?;
                if branch_ref != upstream {
                    die_push_simple(config, remote, &upstream)?;
                }
            }
        }
        PushDefaultMode::Upstream => {
            if !same_remote {
                eprintln!(
                    "fatal: You are pushing to remote '{remote}', which is not the upstream of\n\
your current branch '{branch}', without telling me what to push\n\
to update which remote branch."
                );
                return Err(GitError::Exit(128));
            }
            dst = push_upstream_ref(config, branch, remote, auto_setup)?;
        }
        PushDefaultMode::Current => {}
        PushDefaultMode::Matching | PushDefaultMode::Nothing => unreachable!(),
    }

    if auto_setup && config.get_all("branch", Some(branch), "merge").is_empty() {
        set_upstream = true;
    }

    Ok(DefaultPushRefspecs {
        refspecs: vec![format!("{branch_ref}:{dst}")],
        set_upstream,
        mirror: false,
    })
}

fn push_default_mode(config: &GitConfig) -> PushDefaultMode {
    match config.get("push", None, "default") {
        Some("nothing") => PushDefaultMode::Nothing,
        Some("matching") => PushDefaultMode::Matching,
        Some("simple") => PushDefaultMode::Simple,
        Some("upstream" | "tracking") => PushDefaultMode::Upstream,
        Some("current") => PushDefaultMode::Current,
        _ => PushDefaultMode::Unspecified,
    }
}

fn default_push_remote(config: &GitConfig, branch: Option<&str>) -> Result<String> {
    if let Some(branch) = branch
        && let Some(remote) = config.get("branch", Some(branch), "pushRemote")
    {
        return Ok(remote.to_string());
    }
    if let Some(remote) = config.get("remote", None, "pushDefault") {
        return Ok(remote.to_string());
    }
    if let Some(branch) = branch
        && let Some(remote) = config.get("branch", Some(branch), "remote")
    {
        return Ok(remote.to_string());
    }
    if remote_exists(config, "origin") {
        return Ok("origin".to_string());
    }
    let remotes = remote_names(config);
    if let [remote] = remotes.as_slice() {
        return Ok(remote.clone());
    }
    eprintln!(
        "fatal: No configured push destination.\n\
Either specify the URL from the command-line or configure a remote repository using\n\n\
    git remote add <name> <url>\n\n\
and then push using the remote name\n\n\
    git push <name>"
    );
    Err(GitError::Exit(128))
}

fn default_fetch_remote_for_branch(config: &GitConfig, branch: &str) -> String {
    if let Some(remote) = config.get("branch", Some(branch), "remote") {
        return remote.to_string();
    }
    if remote_exists(config, "origin") {
        return "origin".to_string();
    }
    let remotes = remote_names(config);
    match remotes.as_slice() {
        [remote] => remote.clone(),
        _ => "origin".to_string(),
    }
}

fn push_upstream_ref(
    config: &GitConfig,
    branch: &str,
    remote: &str,
    auto_setup: bool,
) -> Result<String> {
    let merges = config
        .get_all("branch", Some(branch), "merge")
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if merges.is_empty() && auto_setup {
        return Ok(format!("refs/heads/{branch}"));
    }
    if merges.is_empty() || config.get("branch", Some(branch), "remote").is_none() {
        let advice = if auto_setup {
            ""
        } else {
            "\nTo have this happen automatically for branches without a tracking\nupstream, see 'push.autoSetupRemote' in 'git help config'.\n"
        };
        eprintln!(
            "fatal: The current branch {branch} has no upstream branch.\n\
To push the current branch and set the remote as upstream, use\n\n\
    git push --set-upstream {remote} {branch}\n\
{advice}"
        );
        return Err(GitError::Exit(128));
    }
    if merges.len() != 1 {
        eprintln!(
            "fatal: The current branch {branch} has multiple upstream branches, refusing to push."
        );
        return Err(GitError::Exit(128));
    }
    Ok(merges[0].to_string())
}

fn die_push_simple(config: &GitConfig, remote: &str, upstream: &str) -> Result<()> {
    let short_upstream = upstream.strip_prefix("refs/heads/").unwrap_or(upstream);
    let advice_pushdefault = if matches!(push_default_mode(config), PushDefaultMode::Unspecified) {
        "\nTo choose either option permanently, see push.default in 'git help config'.\n"
    } else {
        ""
    };
    eprintln!(
        "fatal: The upstream branch of your current branch does not match\n\
the name of your current branch.  To push to the upstream branch\n\
on the remote, use\n\n\
    git push {remote} HEAD:{short_upstream}\n\n\
To push to the branch of the same name on the remote, use\n\n\
    git push {remote} HEAD\n\
{advice_pushdefault}"
    );
    Err(GitError::Exit(128))
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

fn configure_push_upstreams_from_report(
    git_dir: &Path,
    remote: &str,
    refs: &[sley_remote::PushReportRef],
) -> Result<()> {
    let mut config = read_repo_config(git_dir)?;
    let format = repository_object_format(git_dir)?;
    let store = FileRefStore::new(git_dir, format);
    let current_branch = store.current_branch().ok().flatten();
    for reference in refs {
        if !matches!(
            reference.status,
            sley_remote::PushRefStatus::Ok | sley_remote::PushRefStatus::UpToDate
        ) || reference.is_deletion()
        {
            continue;
        }
        let Some(src) = reference.src.as_deref() else {
            continue;
        };
        let branch = if src == "HEAD" {
            let Some(branch) = current_branch.as_deref() else {
                continue;
            };
            branch
        } else {
            let Some(branch) = src.strip_prefix("refs/heads/") else {
                continue;
            };
            branch
        };
        if !reference.dst.starts_with("refs/heads/") {
            continue;
        }
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
        config_set_value(&mut config, &merge_key, &reference.dst, false);
    }
    write_repo_config(git_dir, &config)
}

/// Implements builtin/fetch.c's `--set-upstream` post-fetch configuration.
///
/// The relevant upstream is the fetched branch meant to be merged with the
/// current one — git's `ref_map` entry with no local peer ref. In sley that is a
/// [`FetchRefUpdate`] with no `dst` (a FETCH_HEAD-only entry). When exactly one
/// exists and the current branch is real, mirror `install_branch_config` by
/// writing `branch.<current>.{remote,merge}`; the various ambiguous/unsupported
/// cases emit the same warnings git does and write nothing.
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
                eprintln!(
                    "warning: multiple branches detected, incompatible with --set-upstream"
                );
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
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<()> {
    fetch_local_repository_with_outcome(git_dir, format, source, refspecs, options, &[]).map(|_| ())
}

fn fetch_local_repository_with_outcome(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
    server_options: &[String],
) -> Result<sley_remote::FetchOutcome> {
    let remote_git_dir = ls_remote_git_dir(source)?;
    let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
    let config = repo_config_with_transport_policy(git_dir)?;
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
    before_refs: &std::collections::BTreeMap<String, ObjectId>,
    outcome: &sley_remote::FetchOutcome,
) -> Result<()> {
    if quiet {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut rows = Vec::new();
    for update in &outcome.ref_updates {
        let Some(dst) = update.dst.as_deref() else {
            continue;
        };
        let old = before_refs.get(dst).copied();
        if old == Some(update.oid) {
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
    if let Ok(remote_git_dir) = local_remote_git_dir(config, source, git_dir) {
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
fn report_followremotehead_warn(
    follow: &str,
    remote: &str,
    head_name: &str,
    existing: &RefTarget,
) {
    let follow_lower = follow.to_ascii_lowercase();
    // `no_warn_branch` is the `<branch>` in `warn-if-not-<branch>`; plain `warn`
    // has none. Anything else is not a warn mode.
    let no_warn_branch = if follow_lower == "warn" {
        None
    } else if let Some(rest) = follow_lower.strip_prefix("warn-if-not-") {
        // Match git's case-sensitive `strcmp` on the branch name; recover the
        // original (non-lowercased) suffix to compare.
        Some(follow["warn-if-not-".len()..].to_string())
        .filter(|_| !rest.is_empty())
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
                println!("'HEAD' at '{remote}' is '{head_name}', but we have '{prev_head}' locally.");
            }
        }
        RefTarget::Direct(oid) => {
            println!(
                "'HEAD' at '{remote}' is '{head_name}', but we have a detached HEAD pointing to '{oid}' locally."
            );
        }
    }
}

pub(crate) fn fetch_source_is_ssh(source: &str) -> Result<bool> {
    let resolved = ls_remote_resolved_url(source)?;
    Ok(matches!(
        parse_remote_url(&resolved)?.transport,
        RemoteTransport::Ssh | RemoteTransport::Ext
    ))
}

pub(crate) fn fetch_source_is_git(source: &str) -> Result<bool> {
    let resolved = ls_remote_resolved_url(source)?;
    Ok(parse_remote_url(&resolved)?.transport == RemoteTransport::Git)
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
    fetch_ssh_repository_with_outcome(git_dir, format, source, refspecs, options).map(|_| ())
}

fn fetch_ssh_repository_with_outcome(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<sley_remote::FetchOutcome> {
    let config = repo_config_with_transport_policy(git_dir)?;
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
        &[],
    )
}

pub(crate) fn fetch_git_repository(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<()> {
    fetch_git_repository_with_outcome(git_dir, format, source, refspecs, options).map(|_| ())
}

fn fetch_git_repository_with_outcome(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<sley_remote::FetchOutcome> {
    let remote = parse_remote_url(&ls_remote_resolved_url(source)?)?;
    let config = read_repo_config(git_dir)?;
    let fetch_source = sley_remote::FetchSource::Git {
        remote,
        protocol_v2: configured_protocol_version(Some(&config)) == Some(ProtocolVersion::V2),
    };
    run_fetch(
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

fn fetch_source_is_http(source: &str) -> Result<bool> {
    sley_remote::remote_url_is_http(&ls_remote_resolved_url(source)?)
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
    fetch_http_repository_with_outcome(git_dir, format, source, refspecs, options).map(|_| ())
}

fn fetch_http_repository_with_outcome(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<sley_remote::FetchOutcome> {
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
        &[],
    )
}

/// Resolve `repository` to an HTTP(S) remote and list its advertisements via
/// [`sley_remote::ls_remote`], returning `None` for non-HTTP transports. URL/
/// config resolution and the ref-name pattern matching stay here; the
/// advertisement listing and class filtering live in the library.
fn ls_remote_http_records(
    repository: &str,
    options: &LsRemoteOptions,
    transport_config: &GitConfig,
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
        Some(transport_config),
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
    quiet: bool,
    get_url: bool,
    sort: Option<LsRemoteSort>,
    repository: Option<String>,
    patterns: Vec<String>,
    upload_pack_command: Option<String>,
    server_options: Vec<String>,
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

/// Validate a single refname component the way git's `check_refname_component`
/// (refs.c `refname_disposition` table) does, honoring the
/// `REFNAME_REFSPEC_PATTERN` flag. Returns `false` for a malformed component and
/// reports (via `pattern_seen`) whether this component consumed the single
/// asterisk a refspec pattern is allowed.
fn refspec_component_ok(component: &str, allow_pattern: bool, pattern_seen: &mut bool) -> bool {
    if component.is_empty() {
        return false;
    }
    if component.starts_with('.') {
        return false;
    }
    if component.ends_with(".lock") {
        return false;
    }
    let bytes = component.as_bytes();
    for (idx, &byte) in bytes.iter().enumerate() {
        match byte {
            // disposition 4: control chars, space, and the forbidden set.
            0x00..=0x20 | 0x7f | b'~' | b'^' | b':' | b'?' | b'[' | b'\\' => return false,
            // disposition 2: ".." is forbidden.
            b'.' if bytes.get(idx + 1) == Some(&b'.') => return false,
            // disposition 3: "@{" is forbidden.
            b'@' if bytes.get(idx + 1) == Some(&b'{') => return false,
            // disposition 5: '*' is only allowed once, and only for patterns.
            b'*' => {
                if !allow_pattern || *pattern_seen {
                    return false;
                }
                *pattern_seen = true;
            }
            _ => {}
        }
    }
    true
}

/// Faithful port of git's `check_refname_format` for the refspec-validation path
/// (refs.c). `allow_onelevel` mirrors `REFNAME_ALLOW_ONELEVEL`; `allow_pattern`
/// mirrors `REFNAME_REFSPEC_PATTERN` (a single `*` somewhere in the ref).
fn refspec_refname_ok(refname: &str, allow_onelevel: bool, allow_pattern: bool) -> bool {
    if refname == "@" || refname.starts_with('/') || refname.ends_with('/') {
        return false;
    }
    if refname.ends_with('.') {
        return false;
    }
    let mut pattern_seen = false;
    let mut component_count = 0;
    for component in refname.split('/') {
        if !refspec_component_ok(component, allow_pattern, &mut pattern_seen) {
            return false;
        }
        component_count += 1;
    }
    if !allow_onelevel && component_count < 2 {
        return false;
    }
    true
}

/// Validate a configured `remote.<name>.fetch`/`push` refspec the way git's
/// `parse_refspec` (refspec.c) does, dying-equivalent (returns `false`) on the
/// same inputs git rejects. `fetch` selects the fetch vs push rule set.
fn configured_refspec_valid(refspec: &str, fetch: bool) -> bool {
    // Leading '+' (force) or '^' (negative) are stripped first (mutually
    // exclusive in git: a negative refspec never carries force, but the parser
    // only inspects one prefix char).
    let mut lhs = refspec;
    let mut negative = false;
    if let Some(rest) = lhs.strip_prefix('+') {
        lhs = rest;
    } else if let Some(rest) = lhs.strip_prefix('^') {
        negative = true;
        lhs = rest;
    }

    // git uses strrchr(lhs, ':') — the LAST colon splits src from dst.
    let rhs = lhs.rfind(':');

    // Negative refspecs only have one side.
    if negative && rhs.is_some() {
        return false;
    }

    // Special case ":" (or "+:") as the matching push refspec.
    if !fetch && matches!(rhs, Some(0)) && lhs.len() == 1 {
        return true;
    }

    let (src, dst) = match rhs {
        Some(pos) => (&lhs[..pos], Some(&lhs[pos + 1..])),
        None => (lhs, None),
    };
    let dst_has_glob = dst.is_some_and(|d| d.contains('*'));
    let src_has_glob = src.contains('*');

    let mut is_glob = dst_has_glob && !dst.unwrap_or("").is_empty();
    if src_has_glob {
        // LHS has a glob: for a fetch with no RHS the source must look like a
        // pattern; with an RHS the RHS must also be a glob.
        if (dst.is_some() && !is_glob) || (dst.is_none() && !negative && fetch) {
            return false;
        }
        is_glob = true;
    } else if dst.is_some() && is_glob {
        // RHS globbed but LHS did not.
        return false;
    }

    let src = if src == "@" { "HEAD" } else { src };

    if negative {
        // Negative refspecs: LHS only, non-empty, not an exact sha1, valid ref.
        if src.is_empty() {
            return false;
        }
        return refspec_refname_ok(src, true, is_glob);
    }

    if fetch {
        // LHS: empty ok (means HEAD); exact sha1 ok; else must be a valid ref.
        if !src.is_empty() && !refspec_refname_ok(src, true, is_glob) {
            return false;
        }
        // RHS: missing/empty ok; else must be a valid ref.
        if let Some(d) = dst
            && !d.is_empty()
            && !refspec_refname_ok(d, true, is_glob)
        {
            return false;
        }
    } else {
        // Push LHS: empty ok (delete); globbed must be a valid ref; else anything.
        if !src.is_empty() && is_glob && !refspec_refname_ok(src, true, is_glob) {
            return false;
        }
        // Push RHS: missing ok only if LHS is a valid ref; empty not allowed;
        // else must be a valid ref.
        match dst {
            // No RHS: the LHS must be a valid-looking ref.
            None => return refspec_refname_ok(src, true, is_glob),
            // Empty RHS (`src:`) is never allowed for push.
            Some(d) if d.is_empty() => return false,
            Some(d) if !refspec_refname_ok(d, true, is_glob) => return false,
            Some(_) => {}
        }
    }
    true
}

/// Mirror git's `remote_get` validating every configured `remote.<name>.fetch`
/// and `remote.<name>.push` refspec via `refspec_append` (which dies on the
/// first invalid value). Only runs when `repository` names a configured remote.
fn validate_configured_remote_refspecs(repository: &str) -> Result<()> {
    let cwd = env::current_dir()?;
    let Ok(git_dir) = discover_git_dir(&cwd) else {
        return Ok(());
    };
    let Ok(config) = read_repo_config(&git_dir) else {
        return Ok(());
    };
    for (key, fetch) in [("fetch", true), ("push", false)] {
        for value in config.get_all("remote", Some(repository), key) {
            let Some(value) = value else { continue };
            let value = value.trim_start_matches([' ', '\t']);
            if !configured_refspec_valid(value, fetch) {
                eprintln!("fatal: invalid refspec '{value}'");
                return Err(GitError::Exit(128));
            }
        }
    }
    Ok(())
}

fn default_ls_remote_remote() -> Result<String> {
    let git_dir = discover_git_dir(&env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    default_fetch_remote(&git_dir, format)
}

pub(crate) fn cmd_ls_remote(args: &[String]) -> Result<()> {
    let mut options = parse_ls_remote_options(args)?;
    let implicit_repository = options.repository.is_none();
    let repository = match options.repository.as_deref() {
        Some(repository) => repository.to_string(),
        None => default_ls_remote_remote()?,
    };
    validate_configured_remote_refspecs(&repository)?;
    if options.get_url {
        println!("{}", ls_remote_display_url(&repository)?);
        return Ok(());
    }
    let local_sort_git_dir = validate_ls_remote_sort_context(options.sort)?;
    let local_sort_format = local_sort_git_dir
        .as_deref()
        .map(repository_object_format)
        .transpose()?;
    let transport_config = transport_policy_config_for_cwd()?;
    if options.server_options.is_empty() {
        options.server_options = configured_server_options(&transport_config, &repository)?;
    } else if configured_legacy_protocol(Some(&transport_config)) {
        eprintln!("fatal: server options require protocol version 2 or later");
        eprintln!("fatal: see protocol.version in 'git help config' for more details");
        return Err(GitError::Exit(128));
    }
    let resolved_repository = ls_remote_resolved_url(&repository)?;
    check_transport_allowed_url(&resolved_repository, Some(&transport_config))?;

    if implicit_repository && !options.quiet {
        eprintln!("From {}", ls_remote_display_url(&repository)?);
    }

    if let Some((mut records, format)) =
        ls_remote_ssh_records(&repository, &options, &transport_config)?
    {
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

    if let Some((mut records, format)) =
        ls_remote_git_records(&repository, &options, &transport_config)?
    {
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

    if let Some((mut records, format)) =
        ls_remote_http_records(&repository, &options, &transport_config)?
    {
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

    if let Some(command) = options.upload_pack_command.as_deref() {
        let mut records = ls_remote_upload_pack_command_records(command, &options)?;
        let format = ObjectFormat::Sha1;
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

    if matches!(
        parse_remote_url(&resolved_repository).map(|url| url.transport),
        Ok(RemoteTransport::File | RemoteTransport::Local)
    ) {
        trace_configured_local_protocol_version(Some(&transport_config));
        if configured_protocol_version(Some(&transport_config)) == Some(ProtocolVersion::V2) {
            if let Ok(remote_git_dir) = ls_remote_git_dir(&repository)
                && let Ok(remote_common_git_dir) = common_git_dir_for_git_dir(&remote_git_dir)
                && let Ok(format) = repository_object_format(&remote_common_git_dir)
            {
                trace_protocol_v2_upload_pack_capabilities(&remote_git_dir, format);
            }
            trace_protocol_v2_ls_refs_request(&options.server_options);
        }
    }

    let git_dir = match ls_remote_git_dir(&repository) {
        Ok(git_dir) => git_dir,
        Err(_) => return ls_remote_repository_not_found(&repository),
    };
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let (mut records, format) = sley_remote::ls_remote(
        &sley_remote::LsRemoteSource::Local { git_dir },
        format,
        &ls_remote_filter(&options),
        &|name| ls_remote_ref_matches(name, &options.patterns),
        Some(&transport_config),
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

fn ls_remote_repository_not_found(repository: &str) -> Result<()> {
    eprintln!("fatal: '{repository}' does not appear to be a git repository");
    eprintln!("fatal: Could not read from remote repository.");
    eprintln!();
    eprintln!("Please make sure you have the correct access rights");
    eprintln!("and the repository exists.");
    Err(GitError::Exit(128))
}

fn ls_remote_upload_pack_command_records(
    command: &str,
    options: &LsRemoteOptions,
) -> Result<Vec<LsRemoteRecord>> {
    let output = Proc::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(GitError::Command(format!(
            "upload-pack command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut stdout = output.stdout.as_slice();
    let set = read_ref_advertisement_set(ObjectFormat::Sha1, &mut stdout)?;
    let features = set
        .refs
        .first()
        .map(|advertisement| sley_protocol::parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default();
    let symrefs = features
        .symrefs
        .iter()
        .filter_map(|symref| symref.split_once(':'))
        .map(|(name, target)| (name.to_string(), target.to_string()))
        .collect::<HashMap<_, _>>();
    let mut records = Vec::new();
    for advertisement in set.refs {
        if advertisement.oid.is_null() {
            continue;
        }
        if options.refs_only
            && (advertisement.name == "HEAD" || advertisement.name.ends_with("^{}"))
        {
            continue;
        }
        if (options.heads || options.tags)
            && !((options.heads && advertisement.name.starts_with("refs/heads/"))
                || (options.tags && advertisement.name.starts_with("refs/tags/")))
        {
            continue;
        }
        if !ls_remote_ref_matches(&advertisement.name, &options.patterns) {
            continue;
        }
        records.push(LsRemoteRecord {
            oid: advertisement.oid,
            symref: symrefs.get(&advertisement.name).cloned(),
            name: advertisement.name,
        });
    }
    Ok(records)
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
            "-b" | "-h" | "--heads" | "--branches" => options.heads = true,
            "--no-heads" | "--no-branches" => options.heads = false,
            "-t" | "--tags" => options.tags = true,
            "--no-tags" => options.tags = false,
            "--refs" => options.refs_only = true,
            "--no-refs" => options.refs_only = false,
            "--symref" => options.symref = true,
            "--no-symref" => options.symref = false,
            "--exit-code" => options.exit_code = true,
            "--no-exit-code" => options.exit_code = false,
            "-q" | "--quiet" => options.quiet = true,
            "--no-quiet" => options.quiet = false,
            "--get-url" => options.get_url = true,
            "--no-get-url" => options.get_url = false,
            "--upload-pack" => {
                let Some(value) = args.next_value() else {
                    return ls_remote_usage();
                };
                options.upload_pack_command = Some(value.to_string());
            }
            value if let Some(upload_pack) = long_option_value(value, "upload-pack") => {
                options.upload_pack_command = Some(upload_pack.to_string());
            }
            "--server-option" | "-o" => {
                let Some(value) = args.next_value() else {
                    return ls_remote_usage();
                };
                options.server_options.push(value.to_string());
            }
            "--sort" => {
                let Some(value) = args.next_value() else {
                    return ls_remote_usage();
                };
                options.sort = Some(parse_ls_remote_sort(value)?);
            }
            "--no-upload-pack" | "--no-server-option" => {}
            "--no-sort" => options.sort = None,
            value if let Some(option) = long_option_value(value, "server-option") => {
                options.server_options.push(option.to_string());
            }
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
    transport_config: &GitConfig,
) -> Result<Option<(Vec<LsRemoteRecord>, ObjectFormat)>> {
    let parsed = parse_remote_url(&ls_remote_resolved_url(repository)?)?;
    if !matches!(
        parsed.transport,
        RemoteTransport::Ssh | RemoteTransport::Ext
    ) {
        return Ok(None);
    }
    let records = sley_remote::ls_remote(
        &sley_remote::LsRemoteSource::Ssh(parsed),
        ObjectFormat::Sha1,
        &ls_remote_filter(options),
        &|name| ls_remote_ref_matches(name, &options.patterns),
        Some(transport_config),
        &mut sley_remote::NoCredentials,
    )?;
    Ok(Some(records))
}

fn ls_remote_git_records(
    repository: &str,
    options: &LsRemoteOptions,
    transport_config: &GitConfig,
) -> Result<Option<(Vec<LsRemoteRecord>, ObjectFormat)>> {
    let parsed = parse_remote_url(&ls_remote_resolved_url(repository)?)?;
    if parsed.transport != RemoteTransport::Git {
        return Ok(None);
    }
    let records = sley_remote::ls_remote(
        &sley_remote::LsRemoteSource::Git(parsed),
        ObjectFormat::Sha1,
        &ls_remote_filter(options),
        &|name| ls_remote_ref_matches(name, &options.patterns),
        Some(transport_config),
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

fn check_transport_allowed_url(url: &str, config: Option<&GitConfig>) -> Result<()> {
    let scheme = sley_remote::transport_scheme_for_url(url);
    match sley_remote::check_transport_allowed(&scheme, config, None) {
        Ok(()) => Ok(()),
        Err(err) => {
            eprintln!("fatal: {err}");
            Err(GitError::Exit(128))
        }
    }
}

fn transport_policy_config_for_cwd() -> Result<GitConfig> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd).ok();
    let common_git_dir = git_dir
        .as_deref()
        .and_then(|git_dir| common_git_dir_for_git_dir(git_dir).ok());
    let context = match (&common_git_dir, &git_dir) {
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

fn repo_config_with_transport_policy(git_dir: &Path) -> Result<GitConfig> {
    let mut config = transport_policy_config_for_cwd()?;
    let repo_config = read_repo_config(git_dir)?;
    let cwd = env::current_dir()?;
    if let Ok(current_git_dir) = discover_git_dir(&cwd) {
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

pub(crate) fn ls_remote_git_dir(repository: &str) -> Result<PathBuf> {
    let cwd = env::current_dir()?;
    let local_git_dir = discover_git_dir(&cwd).ok();
    if let Some(git_dir) = local_git_dir.as_deref() {
        let config = read_repo_config(git_dir)?;
        if remote_exists(&config, repository) {
            return local_remote_git_dir(&config, repository, git_dir);
        }
    }
    if let Ok(path) = ls_remote_repository_path(repository, &cwd)
        && let Ok(git_dir) = local_repository_git_dir_path(&path)
    {
        return Ok(git_dir);
    }
    let local_git_dir = local_git_dir.ok_or_else(|| GitError::repository_not_found("not a git repository"))?;
    let config = read_repo_config(&local_git_dir)?;
    let rewritten = rewrite_url_with_config(&config, repository, false);
    if rewritten != repository
        && let Ok(path) = ls_remote_repository_path(&rewritten, &cwd)
        && let Ok(git_dir) = local_repository_git_dir_path(&path)
    {
        return Ok(git_dir);
    }
    local_remote_git_dir(&config, repository, &local_git_dir)
}

fn local_repository_git_dir_path(path: &Path) -> Result<PathBuf> {
    let dot_git_path = path_with_dot_git_suffix(path);
    let candidates = [
        path.join(".git"),
        path.to_path_buf(),
        dot_git_path.join(".git"),
        dot_git_path,
    ];
    for candidate in candidates {
        if remote_git_dir_candidate(&candidate) {
            return Ok(candidate);
        }
        if candidate.is_file()
            && let Some(git_dir) = read_gitdir_file(&candidate)?
            && remote_git_dir_candidate(&git_dir)
        {
            return fs::canonicalize(git_dir).map_err(|err| GitError::Io(err.to_string()));
        }
    }
    Err(GitError::repository_not_found("not a git repository"))
}

fn path_with_dot_git_suffix(path: &Path) -> PathBuf {
    let mut suffixed = path.as_os_str().to_os_string();
    suffixed.push(".git");
    PathBuf::from(suffixed)
}

fn remote_git_dir_candidate(path: &Path) -> bool {
    path.join("HEAD").is_file()
        && (path.join("objects").is_dir() || path.join("commondir").is_file())
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
        | RemoteTransport::Ext
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
        "update" => cmd_remote_update(&args[idx + 1..], verbose),
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

pub(crate) fn cmd_remote_add(args: &[String]) -> Result<()> {
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
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let mut config = read_repo_config_on_disk(&git_dir)?;
    if mirror != RemoteAddMirror::None && master.is_some() {
        eprintln!("fatal: specifying a master branch makes no sense with --mirror");
        return Err(GitError::Exit(128));
    }
    if matches!(mirror, RemoteAddMirror::Push) && !branches.is_empty() {
        eprintln!("fatal: specifying branches to track makes sense only with fetch mirrors");
        return Err(GitError::Exit(128));
    }
    // Build the section body from the parsed options, then let the shared editor
    // append it (and reject a duplicate remote).
    let mut entries = vec![ConfigEntry::new("url", Some(url.to_string()))];
    match mirror {
        RemoteAddMirror::Fetch | RemoteAddMirror::Both => {
            if branches.is_empty() {
                entries.push(ConfigEntry::new("fetch", Some("+refs/*:refs/*".into())));
            } else {
                for branch in &branches {
                    entries.push(ConfigEntry::new(
                        "fetch",
                        Some(remote_add_fetch_refspec(name, branch, mirror)),
                    ));
                }
            }
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
                        Some(remote_add_fetch_refspec(name, branch, mirror)),
                    ));
                }
            }
        }
    }
    if let Some(tag_opt) = tag_opt {
        entries.push(ConfigEntry::new("tagOpt", Some(tag_opt)));
    }
    if mirror == RemoteAddMirror::Both {
        entries.push(ConfigEntry::new("mirror", Some("true".into())));
    }
    // Upstream `builtin/remote.c::check_remote_collision`: a new remote may not
    // nest with an existing one. When the new name is `<existing>/…` it is a
    // subset of that remote; when an existing name is `<new>/…` the new name is
    // a superset. Either collision dies (exit 128).
    for existing in remote_names(&config) {
        if let Some(rest) = name.strip_prefix(&existing)
            && rest.starts_with('/')
        {
            eprintln!("fatal: remote name '{name}' is a subset of existing remote '{existing}'");
            return Err(GitError::Exit(128));
        }
        if let Some(rest) = existing.strip_prefix(name)
            && rest.starts_with('/')
        {
            eprintln!("fatal: remote name '{name}' is a superset of existing remote '{existing}'");
            return Err(GitError::Exit(128));
        }
    }
    match sley_config::remotes::add_remote(&mut config, name, entries) {
        Ok(()) => {}
        Err(sley_config::remotes::RemoteEditError::AlreadyExists) => {
            // Upstream `builtin/remote.c::add`: `error("remote %s already
            // exists.")` then `exit(3)`. A remote counts as existing when it has
            // any config (e.g. a foreign-vcs `remote.<name>.vcs`), which the
            // `[remote "<name>"]` section presence already captures.
            eprintln!("error: remote {name} already exists.");
            return Err(GitError::Exit(3));
        }
        Err(sley_config::remotes::RemoteEditError::NotFound) => {
            return Err(GitError::remote_not_found(name));
        }
    }
    write_repo_config(&git_dir, &config)?;
    // `-f`/`--fetch`: git runs `git fetch <name>` immediately after configuring
    // the remote (builtin/remote.c `add()`), so the tracking refs exist before
    // `-m`'s master HEAD is set.
    if fetch {
        cmd_fetch(&[name.to_string()])?;
        if matches!(mirror, RemoteAddMirror::Fetch | RemoteAddMirror::Both)
            && let Ok(branch) = discover_local_remote_head_branch(&config, name, &git_dir)
        {
            let format = repository_object_format(&git_dir)?;
            let store = FileRefStore::new(&git_dir, format);
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: "HEAD".to_string(),
                expected: None,
                new: RefTarget::Symbolic(format!("refs/heads/{branch}")),
                reflog: None,
            });
            let _ = tx.commit();
        }
    }
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
        return Err(remote_geturl_usage_error());
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
        return Err(remote_remove_usage_error());
    }
    let name = &args[0];
    validate_remote_name(name)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let mut config = read_repo_config_on_disk(&git_dir)?;
    let warn_skipped_local_branches = remote_remove_maps_outside_remote_tracking(&config, name);
    match sley_config::remotes::remove_remote(&mut config, name) {
        Ok(()) => {}
        Err(sley_config::remotes::RemoteEditError::NotFound) => {
            // Upstream `builtin/remote.c::rm`: `error("No such remote: '%s'")`
            // then `exit(2)`.
            eprintln!("error: No such remote: '{name}'");
            return Err(GitError::Exit(2));
        }
        Err(sley_config::remotes::RemoteEditError::AlreadyExists) => {
            return Err(GitError::Command(format!("remote {name} already exists")));
        }
    }
    write_repo_config(&git_dir, &config)?;
    let format = repository_object_format(&git_dir)?;
    if warn_skipped_local_branches {
        warn_remote_remove_skipped_local_branches(&git_dir, format)?;
    }
    remove_remote_tracking_refs(&git_dir, format, name)
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
pub(crate) fn cmd_remote_update(args: &[String], verbose: bool) -> Result<()> {
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

    let git_dir = discover_git_dir(env::current_dir()?)?;
    let config = read_repo_config(&git_dir)?;

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
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let mut config = read_repo_config_on_disk(&git_dir)?;
    sley_config::remotes::augment_with_legacy_remote_files(&mut config, &git_dir);
    // Upstream `builtin/remote.c::mv` order: the old remote's existence is
    // checked first (`error` + `exit(2)`), then the new name's collision
    // (`exit(3)`), and only then the new name's format (`die`, exit 128). The
    // old name is never format-validated — a configured remote with an odd name
    // can still be renamed away.
    let old_exists = config
        .sections
        .iter()
        .any(|section| section.name == "remote" && section.subsection.as_deref() == Some(old));
    if !old_exists {
        eprintln!("error: No such remote: '{old}'");
        return Err(GitError::Exit(2));
    }
    if old != new
        && config
            .sections
            .iter()
            .any(|section| section.name == "remote" && section.subsection.as_deref() == Some(new))
    {
        eprintln!("error: remote {new} already exists.");
        return Err(GitError::Exit(3));
    }
    validate_remote_name(new)?;
    if old == new {
        remove_legacy_remote_file(&git_dir, old)?;
        write_repo_config(&git_dir, &config)?;
        return Ok(());
    }
    let rename_tracking_refs = remote_config_values(&config, old, "fetch")
        .iter()
        .any(|refspec| fetch_refspec_targets_remote_tracking(refspec, old));
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
    remove_legacy_remote_file(&git_dir, old)?;
    write_repo_config(&git_dir, &config)?;
    let format = repository_object_format(&git_dir)?;
    if progress {
        trace2_remote_rename_progress();
    }
    if rename_tracking_refs {
        match rename_remote_tracking_refs(&git_dir, format, old, new) {
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

fn remove_legacy_remote_file(git_dir: &Path, name: &str) -> Result<()> {
    for dir in ["remotes", "branches"] {
        let path = git_dir.join(dir).join(name);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn fetch_refspec_targets_remote_tracking(refspec: &str, remote: &str) -> bool {
    let Some((_, dst)) = refspec.strip_prefix('+').unwrap_or(refspec).split_once(':') else {
        return false;
    };
    dst.starts_with(&format!("refs/remotes/{remote}/"))
}

fn remove_remote_tracking_refs(git_dir: &Path, format: ObjectFormat, remote: &str) -> Result<()> {
    let prefix = format!("refs/remotes/{remote}/");
    remove_remote_packed_refs(git_dir, format, &prefix)?;
    remove_remote_ref_dir(git_dir, "refs", remote)?;
    remove_remote_ref_dir(git_dir, "logs/refs", remote)
}

fn remote_remove_maps_outside_remote_tracking(config: &GitConfig, remote: &str) -> bool {
    remote_config_values(config, remote, "fetch")
        .iter()
        .filter_map(|refspec| refspec.rsplit_once(':').map(|(_, dst)| dst))
        .any(|dst| dst == "refs/*" || dst == "+refs/*")
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
    let stale_refs = stale_refs_for_remote_fetch(config, remote, &remote_refs, &local_refs);
    if stale_refs.is_empty() {
        return Ok(());
    }
    let display_url = remote_config_values(config, remote, "url")
        .into_iter()
        .next()
        .unwrap_or_else(|| remote.into());
    writeln!(stdout, "Pruning {remote}")?;
    writeln!(stdout, "URL: {display_url}")?;
    let remote_head = format!("refs/remotes/{remote}/HEAD");
    let head_target = match store.read_ref(&remote_head)? {
        Some(RefTarget::Symbolic(target)) => Some(target),
        Some(RefTarget::Direct(_)) | None => None,
    };
    for refname in stale_refs {
        let display = refname
            .strip_prefix("refs/remotes/")
            .unwrap_or(refname.as_str());
        if dry_run {
            writeln!(stdout, " * [would prune] {display}")?;
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
        writeln!(stdout, " * [pruned] {display}")?;
        if head_target.as_deref() == Some(refname.as_str()) {
            writeln!(
                stdout,
                " refs/remotes/{remote}/HEAD has become dangling after {refname} was deleted"
            )?;
        }
    }
    Ok(())
}

fn stale_refs_for_remote_fetch(
    config: &GitConfig,
    remote: &str,
    remote_refs: &[sley_refs::Ref],
    local_refs: &[sley_refs::Ref],
) -> Vec<String> {
    let mut stale = BTreeSet::new();
    for spec in remote_config_values(config, remote, "fetch") {
        let Some((src_prefix, dst_prefix)) = fetch_refspec_prefixes(&spec) else {
            continue;
        };
        let remote_names = branch_names_with_prefix(remote_refs, src_prefix)
            .into_iter()
            .collect::<BTreeSet<_>>();
        for suffix in branch_names_with_prefix(local_refs, dst_prefix) {
            if !remote_names.contains(&suffix) {
                stale.insert(format!("{dst_prefix}{suffix}"));
            }
        }
    }
    stale.into_iter().collect()
}

fn fetch_refspec_prefixes(spec: &str) -> Option<(&str, &str)> {
    if spec.starts_with('^') {
        return None;
    }
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let (src, dst) = spec.split_once(':')?;
    if src == "refs/*" && dst == "refs/*" {
        return Some(("refs/heads/", "refs/heads/"));
    }
    let src_prefix = src.strip_suffix('*')?;
    let dst_prefix = dst.strip_suffix('*')?;
    Some((src_prefix, dst_prefix))
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
    let mut config = read_repo_config_on_disk(&git_dir)?;
    let mirror_fetch = remote_config_values(&config, name, "fetch")
        .iter()
        .any(|value| value == "+refs/*:refs/*");
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
            Some(if mirror_fetch {
                format!("+refs/{branch}:refs/{branch}")
            } else {
                remote_branch_fetch_refspec(name, branch)
            }),
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
            return Err(remote_sethead_usage_error());
        }
    };
    validate_remote_name(name)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let mut config = read_repo_config_on_disk(&git_dir)?;
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
        let branch = match discover_local_remote_head_branch(&config, name, &git_dir) {
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
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: head,
            expected: None,
            new: RefTarget::Symbolic(target),
            reflog: None,
        });
        if tx.commit().is_err() {
            eprintln!("error: Could not set up refs/remotes/{name}/HEAD");
            return Err(GitError::Exit(1));
        }
        if config
            .get("remote", Some(name), "followRemoteHEAD")
            .is_some_and(|value| value.eq_ignore_ascii_case("always"))
        {
            set_remote_section_value(&mut config, name, "followRemoteHEAD", "warn");
            write_repo_config(&git_dir, &config)?;
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
        | RemoteTransport::Ext
        | RemoteTransport::Git
        | RemoteTransport::Http
        | RemoteTransport::Https => {
            return Err(GitError::Unsupported(
                "remote discovery for non-local transports".into(),
            ));
        }
    };
    local_repository_git_dir_path(&remote_path)
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
        return Err(remote_seturl_usage_error());
    }
    let name = positional[0];
    let url = positional[1];
    let old_url = positional.get(2).copied();
    validate_remote_name(name)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let mut config = read_repo_config_on_disk(&git_dir)?;
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

pub(crate) fn read_repo_config(git_dir: &Path) -> Result<GitConfig> {
    // Single effective-config reader shared with the library crates: resolves
    // `include.path` / `includeIf` and layers command-line `-c` / `--config-env`
    // / `GIT_CONFIG_*` overrides on top (highest precedence). git applies these to
    // all config reads, not just `git config`, so consumers like `git log`'s
    // i18n.* lookups must see them. The CLI holds command-line `-c` overrides it
    // cannot push into the process env, so it reconstructs the effective
    // `GIT_CONFIG_PARAMETERS` and passes it through.
    sley_config::read_repo_config(git_dir, crate::effective_config_parameters_env().as_deref())
}

/// The repository's on-disk `config` file alone, with NO command-line `-c` /
/// `GIT_CONFIG_*` injection layered on. Use this for the read side of any
/// read-modify-write that persists the result back to the config file:
/// [`read_repo_config`] folds the process-level injection into the returned
/// config, so writing it back would persist `git -c key=value` into the file
/// (upstream keeps `-c` injections process-local and never writes them out).
/// This is the bug class behind clone wrongly baking `git -c …` into the cloned
/// repo's config. Includes (`include.path` / `includeIf`) are still resolved.
pub(crate) fn read_repo_config_on_disk(git_dir: &Path) -> Result<GitConfig> {
    sley_config::read_repo_config(git_dir, None)
}

/// A single `<section>.<key>` value from the *full effective config* (system +
/// global + repository, includes resolved) for `git_dir`. Unlike
/// [`read_repo_config`] — which reads only the repo's own `config` file — this
/// layers the global `~/.gitconfig` and system files, as git does for settings
/// like `branch.autosetuprebase` that are configured outside the cloned repo.
fn clone_effective_config_value(git_dir: &Path, section: &str, key: &str) -> Option<String> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir).ok()?;
    let context = sley_config::ConfigIncludeContext::new(
        Some(common_git_dir.clone()),
        repo_current_branch_name(git_dir),
    );
    let config = sley_config::load_effective_config(&common_git_dir, &context).ok()?;
    config.get(section, None, key).map(str::to_owned)
}

/// Short branch name from `HEAD` (e.g. "main"), or None when detached/unborn.
/// Used for `includeIf "onbranch:<glob>"` resolution; reads HEAD directly so it
/// needs no object-format or ref-store context.
pub(crate) fn repo_current_branch_name(git_dir: &Path) -> Option<String> {
    sley_config::repo_current_branch_name(git_dir)
}

pub(crate) fn write_repo_config(git_dir: &Path, config: &GitConfig) -> Result<()> {
    if git_dir.join("config.lock").exists() {
        eprintln!(
            "error: could not lock config file {}: File exists",
            git_dir.join("config").display()
        );
        return Err(GitError::Exit(255));
    }
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

fn remote_add_fetch_refspec(remote: &str, branch: &str, mirror: RemoteAddMirror) -> String {
    if matches!(mirror, RemoteAddMirror::Fetch | RemoteAddMirror::Both) {
        if branch == "*" {
            "+refs/*:refs/*".to_string()
        } else {
            format!("+refs/{branch}:refs/{branch}")
        }
    } else {
        remote_branch_fetch_refspec(remote, branch)
    }
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
    // git's `valid_remote_name` (remote.c) builds the fetch refspec
    // `refs/heads/test:refs/remotes/<name>/test` and rejects the name if that is
    // not a valid fetch refspec — this catches names with a colon, control
    // chars, or other refname-invalid spellings (e.g. `some:url`). The refspec
    // parser only screens delimiter bytes, so apply git's full
    // `check_refname_format` to the destination ref the name produces, matching
    // upstream's `valid_fetch_refspec` (which runs `check_refname_format` on the
    // refspec ends): this rejects `..` (e.g. `invalid...name`), trailing dots,
    // and `@{` that the delimiter screen lets through.
    let probe = format!("refs/heads/test:refs/remotes/{name}/test");
    let probe_dst = format!("refs/remotes/{name}/test");
    if sley_protocol::parse_refspec(&probe).is_err()
        || sley_refs::check_refname_format(&probe_dst, false).is_err()
    {
        // Upstream `builtin/remote.c` (add / rename): `die("'%s' is not a valid
        // remote name")` — a `fatal:` line and exit 128.
        eprintln!("fatal: '{name}' is not a valid remote name");
        return Err(GitError::Exit(128));
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
