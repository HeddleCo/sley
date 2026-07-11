//! Clone command and helpers.
#![allow(clippy::expect_used)]

use super::config::{
    clone_effective_config_value, read_repo_config, read_repo_config_on_disk, validate_remote_name,
    write_repo_config,
};
use super::fetch::{
    configured_server_options, fetch_bundle, fetch_local_repository, fetch_source_is_git,
    fetch_source_is_ssh, parse_shallow_since,
    repo_config_with_clone_transport_policy, repo_config_with_transport_policy, run_fetch,
    transport_policy_config_for_clone, transport_policy_config_for_cwd, StdoutProgress,
};
use super::fetch::{check_transport_allowed_url, ls_remote_resolved_url};
use super::pack::{
    configured_legacy_protocol, configured_protocol_version, trace_configured_local_protocol_version,
    trace_protocol_v2_ls_refs_request,
};
use super::resolve::{local_repository_git_dir_path, ls_remote_git_dir};
use super::CLONE_UNBORN_BRANCH;
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
    let mut explicit_origin = false;
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
    let mut progress = None::<bool>;
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
        _ if env::var("GIT_TEST_DEFAULT_REF_FORMAT")
            .is_ok_and(|value| value.eq_ignore_ascii_case("reftable")) =>
        {
            RefStorageFormat::Reftable
        }
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
            "--progress" => progress = Some(true),
            "--no-progress" => progress = Some(false),
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
                let value = normalize_clone_filter(value)?;
                add_clone_filter(&mut partial_clone_filter, &value);
            }
            value if value.starts_with("--filter=") => {
                let value = value
                    .strip_prefix("--filter=")
                    .ok_or_else(|| GitError::Command("clone --filter requires a value".into()))?;
                let value = normalize_clone_filter(value)?;
                add_clone_filter(&mut partial_clone_filter, &value);
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
                explicit_origin = true;
                origin = iter
                    .next()
                    .ok_or_else(|| GitError::Command("clone --origin requires a name".into()))?
                    .to_string();
            }
            "--no-origin" => {
                explicit_origin = true;
                origin = "origin".to_string();
            }
            value if value.starts_with("--origin=") => {
                explicit_origin = true;
                origin = value
                    .strip_prefix("--origin=")
                    .ok_or_else(|| GitError::Command("clone --origin requires a name".into()))?
                    .to_string();
            }
            value if value.starts_with("-o") && !value.starts_with("--") && value.len() > 2 => {
                explicit_origin = true;
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
    if !explicit_origin
        && let Some(default_remote) = clone_default_remote_name_config(&config_overrides)?
        && !default_remote.is_empty()
    {
        origin = default_remote;
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
    let repository_arg = positional[0].clone();
    let cwd = env::current_dir()?;
    let transport_config = transport_policy_config_for_clone()?;
    let rewritten_repository = rewrite_url_with_config(&transport_config, &repository_arg, false);
    let rewrite_applied = rewritten_repository != repository_arg;
    let bundle_source_path = clone_bundle_path(&cwd, &rewritten_repository);
    let destination = positional.get(1).map(PathBuf::from).unwrap_or_else(|| {
        default_clone_directory(&rewritten_repository, bare, bundle_source_path.is_some())
    });
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
        explicit_work_tree().and_then(|value| {
            if value.as_os_str().is_empty() {
                None
            } else {
                Some(if value.is_absolute() {
                    value
                } else {
                    cwd.join(value)
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
    let repository = absolutize_local_clone_source(&cwd, &rewritten_repository);
    let remote_config_url = if rewrite_applied {
        repository_arg.clone()
    } else if let Some(bundle_path) = bundle_source_path.as_deref() {
        bundle_path.to_string_lossy().into_owned()
    } else {
        repository.clone()
    };
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
            remote_url: &remote_config_url,
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

    let reject_shallow_config =
        option_reject_shallow.or(clone_reject_shallow_config(&config_overrides)?);
    if sley_remote::remote_url_is_http(&repository).unwrap_or(false) {
        clone_http_repository(CloneHttpOptions {
            repository: &repository,
            remote_url: &remote_config_url,
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
            progress,
            ssh_options,
            reject_shallow: reject_shallow_config.unwrap_or(false),
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
            remote_url: &remote_config_url,
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
            progress,
            ssh_options,
            reject_shallow: reject_shallow_config.unwrap_or(false),
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
            remote_url: &remote_config_url,
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
            progress,
            ssh_options,
            reject_shallow: reject_shallow_config.unwrap_or(false),
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
    validate_local_clone_source_refs(&remote_common_git_dir, format)?;
    let source_has_promisor = source_repository_has_promisor_remote(&remote_common_git_dir)?;
    if source_has_promisor {
        let clone_lazy_fetch_reenabled = env::var("GIT_NO_LAZY_FETCH").ok().as_deref() == Some("0")
            && crate::global_lazy_fetch_enabled();
        if !clone_lazy_fetch_reenabled {
            eprintln!("fatal: lazy fetching disabled; some objects may be missing");
            return Err(GitError::Exit(128));
        }
        if !run_source_promisor_upload_pack_probe(&remote_common_git_dir)? {
            return Err(GitError::Exit(128));
        }
    }
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
    let raw_detached_remote_head = match &branch_tag_oid {
        Some(oid) => Some(oid.clone()),
        None => remote_head_detached(&remote_common_git_dir, format),
    };
    let source_bare = read_repo_config_on_disk(&remote_common_git_dir)
        .ok()
        .and_then(|config| config.get_bool("core", None, "bare"))
        .unwrap_or(false);
    let branch_at_detached_head = if source_bare && branch.is_none() && branch_tag_oid.is_none() {
        raw_detached_remote_head
            .as_ref()
            .map(|oid| clone_branch_pointing_at(&remote_common_git_dir, format, oid))
            .transpose()?
            .flatten()
    } else {
        None
    };
    let detached_remote_head = if branch_at_detached_head.is_some() {
        None
    } else {
        raw_detached_remote_head
    };
    let remote_head_branch = match (&detached_remote_head, &branch, &branch_at_detached_head) {
        // `--branch=<tag>` checks out detached, but the remote can still have a
        // default HEAD branch, and git writes refs/remotes/<origin>/HEAD when
        // that branch was fetched by the configured refspec.
        (Some(_), _, _) if branch_tag_oid.is_some() => {
            clone_remote_head_branch(&remote_common_git_dir, format)?.unwrap_or_default()
        }
        (_, None, Some(branch)) => branch.clone(),
        (Some(_), None, _) => String::new(),
        _ => clone_remote_head_branch(&remote_common_git_dir, format)?.unwrap_or_default(),
    };
    let alternates = clone_alternates(&remote_git_dir, shared, &reference_alternates)?;
    let source_alternates_git_dir = remote_common_git_dir.clone();
    let revision_oid = revision
        .as_deref()
        .map(|rev| resolve_clone_revision(&remote_common_git_dir, format, rev, &origin))
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
    if reject_shallow_config == Some(true) && remote_common_git_dir.join("shallow").exists() {
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
    let depth = if depth.is_some() && local_mechanism {
        eprintln!("warning: --depth is ignored in local clones; use file:// instead.");
        None
    } else {
        depth
    };
    let deepen_since = if deepen_since.is_some() && (local_mechanism || revision.is_some()) {
        eprintln!("warning: --shallow-since is ignored in local clones; use file:// instead.");
        None
    } else {
        deepen_since
    };
    let deepen_not = if !deepen_not.is_empty() && (local_mechanism || revision.is_some()) {
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
                sley_remote::pack_filter_from_spec_for_clone(filter, &remote_common_git_dir, format)?;
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
                remote_url: &remote_config_url,
                depth,
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
                &remote_config_url,
                fetch_refspec,
                false,
                tag_opt.as_deref(),
                partial_clone_filter.as_deref(),
            )?;
            apply_clone_config_overrides(git_dir, &config_overrides)?;
            apply_clone_default_submodule_path_config(git_dir)?;
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
            initial_branch: CLONE_UNBORN_BRANCH.into(),
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
        if let Some(depth) = depth {
            let remote_db = FileObjectDatabase::from_git_dir(&remote_common_git_dir, format);
            let deepen = sley_remote::compute_local_deepen(
                &remote_db,
                format,
                std::slice::from_ref(revision_oid),
                Vec::new(),
                depth,
                false,
            )?;
            sley_remote::apply_shallow_info(&git_dir, format, &deepen.shallow_info)?;
        }
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
                committer_identity_for_reflog()?,
                format!("clone: from {repository}").into_bytes(),
                &config,
            )?;
            print_clone_detached_head_advice(&config, revision_oid);
            run_clone_post_checkout_hook(&git_dir, revision_oid)?;
        } else {
            sley_worktree::checkout_detached(
                &checkout_destination,
                &git_dir,
                format,
                revision_oid,
                committer_identity_for_reflog()?,
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
        committer: committer_identity_for_reflog()?,
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
        reject_shallow: reject_shallow_config.unwrap_or(false),
    };
    let mut credentials = sley_remote::NoCredentials;
    let mut progress_sink = StdoutProgress::default();
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
                let fetch_refspec = if branch_tag_oid.is_some() {
                    if single_branch {
                        Some(format!(
                            "+refs/tags/{checkout_branch}:refs/tags/{checkout_branch}"
                        ))
                    } else {
                        Some(format!("+refs/heads/*:refs/remotes/{origin}/*"))
                    }
                } else if clone_options.detached_head.is_some() {
                    (!single_branch).then(|| format!("+refs/heads/*:refs/remotes/{origin}/*"))
                } else if single_branch {
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
            progress: &mut progress_sink,
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
        if branch_tag_oid.is_some() {
            let config = read_repo_config(&git_dir)?;
            print_clone_detached_head_advice(&config, new_head);
        }
        run_clone_post_checkout_hook(&git_dir, new_head)?;
    }
    if let Some(separate_git_dir) = separate_git_dir.as_deref() {
        apply_clone_separate_git_dir(&checkout_destination, &git_dir, separate_git_dir)?;
    }
    if !local_source {
        emit_explicit_clone_progress(progress, quiet, outcome.empty);
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
    remote_url: &'a str,
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
    let base = if raw.is_absolute() {
        raw
    } else {
        cwd.join(raw)
    };
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

pub(super) fn path_with_bundle_suffix(path: &Path) -> PathBuf {
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
        options.remote_url,
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
            force: false,
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
            reject_shallow: false,
            deepen_relative: false,
            update_head_ok: false,
            deepen_since: None,
            deepen_not: Vec::new(),
            ssh_options: None,
            atomic: false,
            negotiation_restrict: None,
            negotiation_include: None,
        },
    )?;
    if let Some(branch) = head_branch {
        let store = FileRefStore::new(&git_dir, format);
        let remote_branch = format!("refs/remotes/{}/{branch}", options.origin);
        if let Some(RefTarget::Direct(oid)) = store.read_ref(&remote_branch)? {
            store.create_branch(
                &branch,
                oid,
                committer_identity_for_reflog()?,
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
                    committer_identity_for_reflog()?,
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
    remote_url: &'a str,
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
    progress: Option<bool>,
    ssh_options: sley_remote::SshTransportOptions,
    reject_shallow: bool,
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

/// Clone a repository over smart HTTP(S).
fn clone_http_repository(options: CloneHttpOptions<'_>) -> Result<()> {
    clone_network_repository(options, CloneNetworkTransport::Http)
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
    Http,
    Ssh,
    Git,
}

impl CloneNetworkTransport {
    fn name(self) -> &'static str {
        match self {
            Self::Http => "HTTP",
            Self::Ssh => "SSH",
            Self::Git => "git://",
        }
    }
}

struct NetworkCloneDiscovery {
    advertisements: Vec<sley_protocol::RefAdvertisement>,
    features: sley_protocol::UploadPackFeatures,
    v2_handshake: Option<sley_protocol::TransportHandshake>,
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

    let remote = parse_remote_url(&ls_remote_resolved_url(options.repository)?)?;
    let transport_config = transport_policy_config_for_clone()?;
    if matches!(transport, CloneNetworkTransport::Ssh) {
        trace_configured_local_protocol_version(Some(&transport_config));
    }
    let discovery = match transport {
        CloneNetworkTransport::Http => {
            let client = sley_remote::new_http_client();
            let mut credentials = sley_remote::NoCredentials;
            let discovered = sley_remote::http_service_advertisements(
                &client,
                &remote,
                ObjectFormat::Sha1,
                sley_protocol::GitService::UploadPack,
                &mut credentials,
                Some(&transport_config),
            )?;
            let features = sley_remote::http_upload_pack_features(
                &discovered.set.refs,
                discovered.handshake.as_ref(),
            )?;
            NetworkCloneDiscovery {
                advertisements: discovered.set.refs,
                features,
                v2_handshake: discovered.handshake,
            }
        }
        CloneNetworkTransport::Ssh => {
            let (advertisements, features) =
                sley_remote::ssh_upload_pack_advertisements_with_options(
                    &remote,
                    ObjectFormat::Sha1,
                    options.ssh_options,
                )?;
            NetworkCloneDiscovery {
                advertisements,
                features,
                v2_handshake: None,
            }
        }
        CloneNetworkTransport::Git => {
            let discovered = sley_remote::git_upload_pack_advertisements_with_protocol(
                &remote,
                ObjectFormat::Sha1,
                configured_protocol_version(Some(&transport_config)) == Some(ProtocolVersion::V2),
                Some(&transport_config),
            )?;
            NetworkCloneDiscovery {
                advertisements: discovered.refs,
                features: discovered.features,
                v2_handshake: None,
            }
        }
    };
    let advertisements = discovery.advertisements;
    let features = discovery.features;
    let v2_handshake = discovery.v2_handshake;
    let format = features.object_format.unwrap_or(ObjectFormat::Sha1);
    if format != ObjectFormat::Sha1 && matches!(transport, CloneNetworkTransport::Http) {
        return Err(GitError::Unsupported(format!(
            "cloning {} repositories over HTTP is not supported yet",
            format.name()
        )));
    }
    let mut fetch_filter = None::<sley_odb::PackObjectFilter>;
    let mut configured_partial_clone_filter = None::<&str>;
    if let Some(filter) = options.partial_clone_filter {
        if features.filter {
            if let Some(parsed) = sley_remote::pack_filter_from_spec(filter) {
                fetch_filter = Some(parsed);
                configured_partial_clone_filter = Some(filter);
            }
        } else {
            eprintln!("warning: filtering not recognized by server, ignoring");
        }
    }
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
    let remote_url = options.remote_url;
    let template = options.template;
    let template_config = options.template_config;
    let tag_opt = options.tag_opt;
    let config_overrides = options.config_overrides;
    let submodule_active = options.submodule_active;
    let http_remote = matches!(transport, CloneNetworkTransport::Http).then(|| remote.clone());
    let remote_source = match transport {
        CloneNetworkTransport::Http => sley_remote::CloneSource::Http(remote.clone()),
        CloneNetworkTransport::Ssh => sley_remote::CloneSource::Ssh(remote),
        CloneNetworkTransport::Git => sley_remote::CloneSource::Git {
            remote,
            protocol_v2: configured_protocol_version(Some(&transport_config))
                == Some(ProtocolVersion::V2),
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
        committer: committer_identity_for_reflog()?,
        detached_head: None,
        checkout: options.checkout,
        filter: fetch_filter,
        branch_explicit,
        ref_storage: options.ref_storage,
        ssh_options: matches!(transport, CloneNetworkTransport::Ssh).then_some(options.ssh_options),
        reject_shallow: options.reject_shallow,
    };
    let mut credentials = sley_remote::NoCredentials;
    let mut progress = StdoutProgress::new(options.quiet);
    let http_client = matches!(transport, CloneNetworkTransport::Http)
        .then(sley_remote::new_http_client);
    let prefetch_handshake = v2_handshake.clone();
    // Junk-directory cleanup: upstream builtin/clone.c registers `remove_junk`
    // (atexit + signal) that removes the working tree / gitdir it created when the
    // clone dies, unless the destination pre-existed (in which case only its
    // contents are cleared, keeping the toplevel). A failed network clone must not
    // leave a partially populated destination behind — t5601's reject-shallow test
    // reruns the clone into the same path and would otherwise hit "destination path
    // already exists". Record what existed before we create anything.
    let dest_pre_existed = options.destination.exists();
    let git_dir_override_pre_existed = options.git_dir_override.map(Path::exists);
    let clone_result = (|| -> Result<()> {
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
                    remote_url,
                    fetch_refspec,
                    false,
                    tag_opt,
                    configured_partial_clone_filter,
                )?;
                apply_clone_config_overrides(git_dir, config_overrides)?;
                apply_clone_submodule_active(git_dir, submodule_active)?;
                let config = repo_config_with_clone_transport_policy(git_dir)?;
                if options.bundle_uri.is_none()
                    && matches!(transport, CloneNetworkTransport::Http)
                    && let (Some(client), Some(remote), Some(handshake)) =
                        (&http_client, &http_remote, &prefetch_handshake)
                    && sley_remote::transfer_bundle_uri_enabled(&config)
                    && sley_remote::handshake_advertises_bundle_uri(handshake)
                {
                    // Bundle-URI auto-discovery is best-effort: upstream
                    // builtin/clone.c ignores the return value of
                    // `fetch_bundle_uri()` and at most warns, continuing the clone
                    // with the normal negotiation. Any discovery/prefetch failure
                    // here must therefore never abort the clone.
                    let mut bundle_credentials = sley_remote::NoCredentials;
                    let prefetch = sley_remote::http_remote_bundle_uri_list(
                        client,
                        remote,
                        handshake,
                        &mut bundle_credentials,
                        Some(&config),
                    )
                    .and_then(|list| {
                        sley_remote::prefetch_advertised_bundle_uris(git_dir, format, &list)
                    });
                    if let Err(err) = prefetch {
                        eprintln!("warning: failed to fetch bundle URIs: {err}");
                    }
                }
                Ok(config)
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
    emit_explicit_clone_progress(options.progress, options.quiet, empty);
    // An empty-repository clone stops before the checkout that would print
    // "done."; git emits only the warning in that case.
    if !options.quiet && !empty {
        eprintln!("done.");
    }
    Ok(())
    })();
    if clone_result.is_err() {
        remove_clone_junk_directory(options.destination, dest_pre_existed);
        if let (Some(git_dir_override), Some(false)) =
            (options.git_dir_override, git_dir_override_pre_existed)
        {
            remove_clone_junk_directory(git_dir_override, false);
        }
    }
    clone_result
}

/// Remove the working tree / gitdir a failed clone created, mirroring upstream
/// `builtin/clone.c::remove_junk`. When the destination pre-existed (git only
/// permits an empty pre-existing directory), its contents are cleared but the
/// toplevel is kept (`REMOVE_DIR_KEEP_TOPLEVEL`); otherwise the directory the
/// clone created is removed entirely.
fn remove_clone_junk_directory(path: &Path, pre_existed: bool) {
    if !pre_existed {
        let _ = fs::remove_dir_all(path);
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            let _ = fs::remove_dir_all(&entry_path);
        } else {
            let _ = fs::remove_file(&entry_path);
        }
    }
}

fn emit_explicit_clone_progress(progress: Option<bool>, quiet: bool, empty: bool) {
    if progress == Some(true) && !quiet && !empty {
        eprintln!("Receiving objects: 100% (0/0), done.");
    }
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
        options.remote_url,
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

    let config = repo_config_with_clone_transport_policy(&git_dir)?;
    let source = match transport {
        CloneNetworkTransport::Http => sley_remote::FetchSource::Http(remote),
        CloneNetworkTransport::Ssh => sley_remote::FetchSource::Ssh(remote),
        CloneNetworkTransport::Git => sley_remote::FetchSource::Git {
            remote,
            protocol_v2: configured_protocol_version(Some(&config)) == Some(ProtocolVersion::V2),
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
            force: false,
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
            reject_shallow: options.reject_shallow,
            deepen_relative: false,
            update_head_ok: true,
            deepen_since: None,
            deepen_not: Vec::new(),
            record_promisor_refs: false,
            refetch: false,
            ssh_options: None,
            atomic: false,
            negotiation_restrict: None,
            negotiation_include: None,
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

pub(super) fn validate_local_clone_source_refs(git_dir: &Path, format: ObjectFormat) -> Result<()> {
    let refs_dir = git_dir.join("refs");
    if !refs_dir.exists() {
        return Ok(());
    }
    validate_local_clone_source_ref_dir(format, &refs_dir, "refs")
}

fn validate_local_clone_source_ref_dir(
    format: ObjectFormat,
    dir: &Path,
    prefix: &str,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
        if path.is_dir() {
            validate_local_clone_source_ref_dir(format, &path, &name)?;
            continue;
        }
        if name.ends_with(".lock") {
            continue;
        }
        let bytes = fs::read(&path)?;
        if local_clone_ref_bytes_are_reftable_sentinel(&name, &bytes) {
            continue;
        }
        let reference = match sley_refs::parse_loose_ref(format, name.clone(), &bytes) {
            Ok(reference) => reference,
            Err(GitError::InvalidFormat(message)) => {
                eprintln!("fatal: {message}");
                return Err(GitError::Exit(128));
            }
            Err(err) => return Err(err),
        };
        if let sley_refs::RefTarget::Direct(oid) = reference.target
            && oid.is_null()
        {
            eprintln!("fatal: reference {name} points to null OID");
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

fn local_clone_ref_bytes_are_reftable_sentinel(name: &str, bytes: &[u8]) -> bool {
    name == "refs/heads" && bytes == b"this repository uses the reftable format\n"
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
    normalize_clone_filter(value).map(|_| ())
}

pub(super) fn normalize_clone_filter(value: &str) -> Result<String> {
    if value == "blob:none" {
        return Ok(value.to_string());
    }
    if let Some(depth) = value.strip_prefix("tree:") {
        let depth = parse_rev_list_tree_depth(depth)?;
        return Ok(format!("tree:{depth}"));
    }
    if let Some(limit) = value.strip_prefix("blob:limit=") {
        let limit = git_parse_blob_limit(limit).ok_or_else(|| {
            eprintln!("fatal: invalid filter-spec 'blob:limit={limit}'");
            GitError::Exit(128)
        })?;
        return Ok(format!("blob:limit={limit}"));
    }
    if let Some(object_type) = value.strip_prefix("object:type=") {
        parse_rev_list_object_type_filter(object_type)?;
        return Ok(value.to_string());
    }
    if value.starts_with("sparse:oid=") {
        return Ok(value.to_string());
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

pub(super) fn validate_upload_pack_filter_config() -> Result<()> {
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

pub(super) fn trace_index_pack_fsck_objects_if_configured() {
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

pub(super) fn trace_pack_objects_filter(filter: Option<&str>) {
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
    remote_url: &'a str,
    depth: Option<u32>,
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
        CLONE_UNBORN_BRANCH
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
        options.remote_url,
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
            force: false,
            append: false,
            write_fetch_head: false,
            tag_option_explicit: options.tag_opt.is_some(),
            prune_option_explicit: false,
            prune_tags_option_explicit: false,
            refmap: None,
            depth: options.depth,
            merge_srcs: Vec::new(),
            filter: options.fetch_filter,
            refetch: false,
            cloning: true,
            record_promisor_refs: true,
            update_shallow: false,
            reject_shallow: false,
            deepen_relative: false,
            update_head_ok: false,
            deepen_since: None,
            deepen_not: Vec::new(),
            ssh_options: None,
            atomic: false,
            negotiation_restrict: None,
            negotiation_include: None,
        },
    );
    env::set_current_dir(previous_cwd)?;
    fetch_result?;
    if options.copy_source_alternates {
        let source_git_dir = common_git_dir_for_git_dir(&ls_remote_git_dir(options.repository)?)?;
        install_local_clone_objects(&source_git_dir, &git_dir, options.local_object_install)?;
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
pub(super) fn parse_clone_config_override(value: &str) -> Result<GlobalConfigOverride> {
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

fn clone_default_remote_name_config(overrides: &[GlobalConfigOverride]) -> Result<Option<String>> {
    if let Some(value) = overrides
        .iter()
        .rev()
        .find(|entry| entry.key.eq_ignore_ascii_case("clone.defaultRemoteName"))
        .map(|entry| entry.value.clone())
    {
        return Ok(Some(value));
    }
    if let Ok(Some(value)) = global_config_value("clone.defaultRemoteName") {
        return Ok(Some(value));
    }
    let context = sley_config::ConfigIncludeContext::new(None, None);
    let mut config =
        sley_config::load_pre_dispatch_config(None, &context).map_err(report_config_setup_error)?;
    let parameters = injected_config_parameters()?;
    let base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        &base,
    )
    .map_err(report_config_setup_error)?;
    Ok(config
        .get("clone", None, "defaultRemoteName")
        .map(str::to_owned))
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

fn apply_clone_alternates(git_dir: &Path, alternates: &[PathBuf], _dissociate: bool) -> Result<()> {
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
        if let Some(oid) = resolve_clone_ref_oid(&store, reference.target)?
            && seen.insert(oid)
        {
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
    if fs::symlink_metadata(&source_objects)?
        .file_type()
        .is_symlink()
    {
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
    if clone_sticky_recursive_clone_config()? {
        let recurse_key = parse_config_key("submodule.recurse")?;
        config_set_value(&mut config, &recurse_key, "true", false);
    }
    write_repo_config(git_dir, &config)
}

fn clone_sticky_recursive_clone_config() -> Result<bool> {
    if let Ok(Some(value)) = global_config_value("submodule.stickyRecursiveClone") {
        return Ok(parse_config_bool(&value).unwrap_or(false));
    }
    let context = sley_config::ConfigIncludeContext::new(None, None);
    let mut config =
        sley_config::load_pre_dispatch_config(None, &context).map_err(report_config_setup_error)?;
    let parameters = injected_config_parameters()?;
    let base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        &base,
    )
    .map_err(report_config_setup_error)?;
    Ok(config
        .get("submodule", None, "stickyRecursiveClone")
        .and_then(parse_config_bool)
        .unwrap_or(false))
}

fn apply_clone_default_submodule_path_config(git_dir: &Path) -> Result<()> {
    if !crate::clone_init_default_submodule_path_config()? {
        return Ok(());
    }
    let config = GitConfig::read(git_dir.join("config")).unwrap_or_default();
    if config.get_bool("extensions", None, "submodulePathConfig") == Some(false) {
        return Ok(());
    }
    crate::enable_submodule_path_config_extension(git_dir)
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
        let git_dir = crate::session::cli_git_dir_from(destination)?;
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
    let submodules = crate::commands::submodule::read_submodule_configs(destination)?;
    if submodules.is_empty() {
        return Ok(());
    }
    let pathspecs: Vec<&String> = if recurse_all {
        Vec::new()
    } else {
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

fn print_clone_detached_head_advice(config: &GitConfig, oid: &ObjectId) {
    if !config
        .get_bool("advice", None, "detachedHead")
        .unwrap_or(true)
    {
        return;
    }
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
    let store = FileRefStore::new_without_reference_backend_env(remote_common_git_dir, format);
    if store
        .read_ref(&format!("refs/heads/{name}"))
        .ok()?
        .is_some()
    {
        return None;
    }
    let tag_ref = format!("refs/tags/{name}");
    let target = store.read_ref(&tag_ref).ok()??;
    let oid = resolve_clone_ref_oid(&store, target).ok()??;
    let db = FileObjectDatabase::from_git_dir(remote_common_git_dir, format);
    peel_clone_revision_to_commit(&db, format, &oid).ok()
}

fn resolve_clone_revision(
    remote_common_git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
    origin: &str,
) -> Result<ObjectId> {
    let db = FileObjectDatabase::from_git_dir(remote_common_git_dir, format);
    let oid = if rev.len() == format.hex_len() && rev.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        ObjectId::from_hex(format, rev).map_err(|_| clone_revision_not_found(rev, origin))?
    } else if rev == "HEAD" || rev.starts_with("refs/") {
        let store = FileRefStore::new_without_reference_backend_env(remote_common_git_dir, format);
        let target = store
            .read_ref(rev)?
            .ok_or_else(|| clone_revision_not_found(rev, origin))?;
        resolve_clone_ref_oid(&store, target)?
            .ok_or_else(|| clone_revision_not_found(rev, origin))?
    } else {
        return Err(clone_revision_not_found(rev, origin));
    };
    peel_clone_revision_to_commit(&db, format, &oid)
}

fn clone_revision_not_found(rev: &str, origin: &str) -> GitError {
    eprintln!("fatal: Remote revision {rev} not found in upstream {origin}");
    GitError::Exit(128)
}

fn peel_clone_revision_to_commit<R: ObjectReader>(
    db: &R,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(oid)?;
    match object.object_type {
        ObjectType::Commit => Ok(*oid),
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body)?;
            peel_clone_revision_to_commit(db, format, &tag.object)
        }
        other => {
            eprintln!("error: object {oid} is a {}, not a commit", other.as_str());
            Err(GitError::Exit(128))
        }
    }
}

fn remote_head_branch(remote_git_dir: &Path, format: ObjectFormat) -> Result<String> {
    let remote_store = FileRefStore::new_without_reference_backend_env(remote_git_dir, format);
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
    let remote_store = FileRefStore::new_without_reference_backend_env(remote_git_dir, format);
    let Some(RefTarget::Symbolic(target)) = remote_store.read_ref("HEAD")? else {
        return Ok(None);
    };
    let Some(branch) = target.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    if remote_store.read_ref(&target)?.is_some() {
        return Ok(Some(branch.to_string()));
    }
    if remote_store
        .list_refs()?
        .into_iter()
        .any(|reference| reference.name.starts_with("refs/heads/"))
    {
        return Ok(None);
    }
    Ok(Some(branch.to_string()))
}

fn clone_branch_pointing_at(
    remote_git_dir: &Path,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<String>> {
    let remote_store = FileRefStore::new_without_reference_backend_env(remote_git_dir, format);
    let preferred = clone_default_branch_name();
    let mut first_match = None;
    for reference in remote_store.list_refs()? {
        let Some(branch) = reference.name.strip_prefix("refs/heads/") else {
            continue;
        };
        let points_at_oid = match reference.target {
            RefTarget::Direct(target) => &target == oid,
            RefTarget::Symbolic(_) => {
                remote_store.read_ref(&reference.name)? == Some(RefTarget::Direct(*oid))
            }
        };
        if !points_at_oid {
            continue;
        }
        if branch == preferred {
            return Ok(Some(branch.to_string()));
        }
        first_match.get_or_insert_with(|| branch.to_string());
    }
    Ok(first_match)
}

/// The remote `HEAD` commit when it is detached (no default branch).
pub(super) fn remote_head_detached(remote_git_dir: &Path, format: ObjectFormat) -> Option<ObjectId> {
    let remote_store = FileRefStore::new_without_reference_backend_env(remote_git_dir, format);
    match remote_store.read_ref("HEAD").ok()? {
        Some(RefTarget::Direct(oid)) => Some(oid),
        _ => None,
    }
}

fn trace2_config_param_matches(pattern: &str, key: &str) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    let key = key.to_ascii_lowercase();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" || pattern == key {
        return true;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return false;
    }
    let mut remainder = key.as_str();
    if let Some(first) = parts.first().filter(|part| !part.is_empty()) {
        let Some(stripped) = remainder.strip_prefix(first) else {
            return false;
        };
        remainder = stripped;
    }
    for part in parts
        .iter()
        .skip(1)
        .take(parts.len().saturating_sub(2))
        .filter(|part| !part.is_empty())
    {
        let Some(idx) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[idx + part.len()..];
    }
    if let Some(last) = parts.last().filter(|part| !part.is_empty()) {
        return remainder.ends_with(last);
    }
    true
}

fn trace2_config_params_include(config: &GitConfig, key: &str) -> bool {
    let env_params = env::var("GIT_TRACE2_CONFIG_PARAMS")
        .ok()
        .filter(|value| !value.is_empty());
    let config_params = config
        .get("trace2", None, "configParams")
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    env_params
        .into_iter()
        .chain(config_params)
        .flat_map(|params| {
            params
                .split(',')
                .map(str::trim)
                .filter(|pattern| !pattern.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .any(|pattern| trace2_config_param_matches(&pattern, key))
}

fn trace2_clone_remote_url(name: &str, url: &str) {
    if env::var_os("GIT_TRACE2").is_none() && env::var_os("GIT_TRACE2_PERF").is_none() {
        return;
    }
    let key = format!("remote.{name}.url");
    let Ok(config) = transport_policy_config_for_cwd() else {
        return;
    };
    if trace2_config_params_include(&config, &key) {
        sley_core::trace2::def_param(&key, url);
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
        entries.push(ConfigEntry::new("tagOpt", Some(tag_opt.to_string())));
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
    write_repo_config(git_dir, &config)?;
    trace2_clone_remote_url(name, url);
    Ok(())
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
pub(super) fn register_promisor_remote(git_dir: &Path, name: &str, filter_spec: &str) -> Result<()> {
    let mut config = read_repo_config_on_disk(git_dir)?;
    if config.get("remote", Some(name), "url").is_none() {
        return Ok(());
    }
    if config.get_bool("remote", Some(name), "promisor") == Some(true)
        && config
            .get("remote", Some(name), "partialclonefilter")
            .is_some()
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

fn source_repository_has_promisor_remote(git_dir: &Path) -> Result<bool> {
    let config = read_repo_config(git_dir)?;
    if config.get("extensions", None, "partialclone").is_some() {
        return Ok(true);
    }
    Ok(remote_names(&config).into_iter().any(|remote| {
        config
            .get_bool("remote", Some(&remote), "promisor")
            .unwrap_or(false)
    }))
}

fn run_source_promisor_upload_pack_probe(git_dir: &Path) -> Result<bool> {
    let config = read_repo_config(git_dir)?;
    for remote in crate::promisor_remote_names(&config) {
        let Some(command) = config.get("remote", Some(&remote), "uploadpack") else {
            continue;
        };
        let Some(url) = config.get("remote", Some(&remote), "url") else {
            continue;
        };
        return crate::prefetch_via_configured_upload_pack(command, url);
    }
    Ok(true)
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
