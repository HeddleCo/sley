//! CLI wiring for user-owned Git remote helpers.
//!
//! Protocol discovery and parsing live in `sley-remote`; this module supplies
//! the CLI runtime services: trace rendering and native Sley fast-import.

use super::fetch::{
    StdoutProgress, check_transport_allowed_url, repo_config_with_transport_policy,
};
use crate::*;
use sley::plumbing::sley_remote::{FetchOptions, RemoteHelperRefValue};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) fn fetch_with_remote_helper(
    git_dir: &Path,
    format: ObjectFormat,
    source: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<Option<sley_remote::FetchOutcome>> {
    let config = repo_config_with_transport_policy(git_dir)?;
    let Some(spec) = sley_remote::resolve_remote_helper(&config, source) else {
        return Ok(None);
    };
    check_transport_allowed_url(spec.url.as_deref().unwrap_or(source), Some(&config))?;
    sley_remote::check_transport_allowed(&spec.name, Some(&config), None).map_err(|error| {
        eprintln!("fatal: {error}");
        GitError::Exit(128)
    })?;
    trace_remote_helper(&spec);
    let mut session = sley_remote::RemoteHelperSession::start(spec.clone(), git_dir, format)?;
    let capabilities = session.capabilities().clone();
    let refs = session.list()?;
    if capabilities.refspecs.is_empty() {
        eprintln!("warning: this remote helper should implement refspec capability");
    }
    let import_refs = refs
        .iter()
        .filter_map(|reference| {
            (!matches!(reference.value, RemoteHelperRefValue::Symbolic(_)))
                .then(|| reference.name.clone())
        })
        .collect::<Vec<_>>();
    let source_refs_before_import = if capabilities.refspecs.is_empty() {
        Vec::new()
    } else {
        let store = FileRefStore::new(git_dir, format);
        import_refs
            .iter()
            .map(|name| store.read_ref(name).map(|target| (name.clone(), target)))
            .collect::<Result<Vec<_>>>()?
    };
    let stream = match session.import(&import_refs) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("error: error while running fast-import");
            return Err(error);
        }
    };
    let stream = sley_remote::rewrite_remote_helper_import_stream(&stream, &capabilities.refspecs)?;
    run_native_fast_import(git_dir, &stream)?;
    let (advertisements, head_symref) =
        sley_remote::imported_remote_helper_advertisements(git_dir, format, &capabilities, &refs)?;
    restore_helper_import_source_refs(git_dir, format, &source_refs_before_import)?;
    let ref_hook = crate::commands::refs::ReferenceTransactionHookRunner::new(git_dir);
    let mut credentials = sley_remote::NoCredentials;
    let mut progress = StdoutProgress;
    sley_remote::finalize_remote_helper_fetch(
        sley_remote::RemoteHelperFetchRequest {
            git_dir,
            format,
            config: &config,
            remote_name: source,
            advertisements: &advertisements,
            head_symref,
            refspecs,
            options: &options,
        },
        sley_remote::FetchServices {
            credentials: &mut credentials,
            progress: &mut progress,
            ref_hook: Some(&ref_hook),
        },
    )
    .map(Some)
}

pub(super) struct RemoteHelperPushOptions {
    pub force: bool,
    pub quiet: bool,
    pub dry_run: bool,
}

pub(super) fn push_with_remote_helper(
    git_dir: &Path,
    format: ObjectFormat,
    remote: &str,
    refspecs: &[String],
    options: RemoteHelperPushOptions,
) -> Result<Option<()>> {
    let config = super::config::read_repo_config(git_dir)?;
    let Some(spec) = sley_remote::resolve_remote_helper(&config, remote) else {
        return Ok(None);
    };
    sley_remote::check_transport_allowed(&spec.name, Some(&config), None).map_err(|error| {
        eprintln!("fatal: {error}");
        GitError::Exit(128)
    })?;
    trace_remote_helper(&spec);
    let mut session = sley_remote::RemoteHelperSession::start(spec.clone(), git_dir, format)?;
    let capabilities = session.capabilities().clone();
    let _remote_refs = session.list()?;
    if capabilities.refspecs.is_empty() {
        eprintln!("fatal: remote-helper doesn't support push; refspec needed");
        return Err(GitError::Exit(128));
    }
    // Git v2.55's import/export helper path still treats a marks-free export as
    // unsupported (t5801 records this as its one known breakage). Preserve the
    // oracle behavior instead of reporting a fixed TODO and making the script
    // itself fail with "known breakage vanished".
    if capabilities.import_marks.is_none() && capabilities.export_marks.is_none() {
        eprintln!("fatal: remote-helper export requires marks");
        return Err(GitError::Exit(128));
    }
    if options.dry_run {
        return Ok(Some(()));
    }
    if options.force {
        let _ = session.set_option("force", "true")?;
    }
    let refspecs = expand_helper_push_refspecs(git_dir, format, refspecs)?;
    let marks_snapshot = snapshot_marks(capabilities.export_marks.as_deref())?;
    let stream = run_native_fast_export(git_dir, format, &capabilities, &refspecs)?;
    let response = match session.export(&stream) {
        Ok(response) => response,
        Err(error) => {
            restore_marks(marks_snapshot)?;
            return Err(error);
        }
    };
    let mut failed = false;
    let mut successful = Vec::new();
    for line in response {
        if let Some(reference) = line.strip_prefix("ok ") {
            successful.push(reference.to_string());
        } else if let Some(rest) = line.strip_prefix("error ") {
            failed = true;
            eprintln!("error: remote helper rejected {rest}");
        }
    }
    if failed {
        return Err(GitError::Exit(1));
    }
    update_helper_push_tracking_refs(
        git_dir,
        format,
        &config,
        remote,
        &capabilities,
        &refspecs,
        &successful,
    )?;
    if !options.quiet {
        eprintln!("To {}", spec.url.as_deref().unwrap_or(remote));
    }
    Ok(Some(()))
}

fn expand_helper_push_refspecs(
    git_dir: &Path,
    format: ObjectFormat,
    refspecs: &[String],
) -> Result<Vec<String>> {
    let store = FileRefStore::new(git_dir, format);
    let refs = store.list_refs()?;
    let mut expanded = Vec::new();
    for raw in refspecs {
        let force = raw.starts_with('+');
        let normalized = sley_remote::normalize_push_refspec(raw);
        let body = normalized.strip_prefix('+').unwrap_or(&normalized);
        let (src, dst) = body.split_once(':').unwrap_or((body, body));
        let (Some((src_prefix, src_suffix)), Some((dst_prefix, dst_suffix))) =
            (src.split_once('*'), dst.split_once('*'))
        else {
            expanded.push(normalized);
            continue;
        };
        for reference in &refs {
            let Some(stem) = reference
                .name
                .strip_prefix(src_prefix)
                .and_then(|rest| rest.strip_suffix(src_suffix))
            else {
                continue;
            };
            let mapped = format!(
                "{}{}:{}{}{}",
                if force { "+" } else { "" },
                reference.name,
                dst_prefix,
                stem,
                dst_suffix
            );
            expanded.push(mapped);
        }
    }
    Ok(expanded)
}

fn run_native_fast_export(
    git_dir: &Path,
    format: ObjectFormat,
    capabilities: &sley_remote::RemoteHelperCapabilities,
    refspecs: &[String],
) -> Result<Vec<u8>> {
    let executable = native_sley_executable()?;
    let mut command = Command::new(&executable);
    command
        .arg("fast-export")
        .arg("--use-done-feature")
        .arg(if capabilities.signed_tags {
            "--signed-tags=verbatim"
        } else {
            "--signed-tags=warn-strip"
        })
        .env("GIT_DIR", git_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(path) = capabilities.import_marks.as_deref() {
        command.arg(format!("--import-marks-if-exists={path}"));
    }
    if let Some(path) = capabilities.export_marks.as_deref() {
        command.arg(format!("--export-marks={path}"));
    }
    let store = FileRefStore::new(git_dir, format);
    let mut has_source = false;
    for refspec in refspecs {
        let body = refspec.strip_prefix('+').unwrap_or(refspec);
        let source = body.split_once(':').map_or(body, |(source, _)| source);
        let destination = body
            .split_once(':')
            .map_or(body, |(_, destination)| destination);
        let export_source = if source == "HEAD" {
            store
                .current_branch_ref()?
                .unwrap_or_else(|| source.to_string())
        } else {
            source.to_string()
        };
        command.arg(format!("--refspec={export_source}:{destination}"));
        if !source.is_empty() {
            command.arg(source);
            has_source = true;
        }
    }
    if !has_source && refspecs.is_empty() {
        return Err(GitError::Command(
            "remote-helper push has no refspecs".into(),
        ));
    }
    let output = command.output().map_err(|err| {
        GitError::Command(format!(
            "could not start native Sley fast-export '{}': {err}",
            PathBuf::from(&executable).display()
        ))
    })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitError::Exit(output.status.code().unwrap_or(1)))
    }
}

struct MarksSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
}

fn snapshot_marks(path: Option<&str>) -> Result<Option<MarksSnapshot>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    let contents = match std::fs::read(&path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok(Some(MarksSnapshot { path, contents }))
}

fn restore_marks(snapshot: Option<MarksSnapshot>) -> Result<()> {
    let Some(snapshot) = snapshot else {
        return Ok(());
    };
    match snapshot.contents {
        Some(contents) => std::fs::write(snapshot.path, contents)?,
        None => match std::fs::remove_file(snapshot.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        },
    }
    Ok(())
}

fn update_helper_push_tracking_refs(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    remote: &str,
    capabilities: &sley_remote::RemoteHelperCapabilities,
    refspecs: &[String],
    successful: &[String],
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let private_mappings = capabilities
        .refspecs
        .iter()
        .map(|spec| sley_protocol::parse_refspec(spec))
        .collect::<Result<Vec<_>>>()?;
    let tracking_mappings = config
        .get_all("remote", Some(remote), "fetch")
        .into_iter()
        .flatten()
        .map(sley_protocol::parse_refspec)
        .collect::<Result<Vec<_>>>()?;
    for refspec in refspecs {
        let normalized = sley_remote::normalize_push_refspec(refspec);
        let body = normalized.strip_prefix('+').unwrap_or(&normalized);
        let (source, destination) = body.split_once(':').unwrap_or((body, body));
        if !successful.iter().any(|name| name == destination) {
            continue;
        }
        let oid = if source.is_empty() {
            None
        } else {
            sley_refs::resolve_ref_peeled(&store, source)?
        };
        let mut destinations = Vec::new();
        if !capabilities.no_private_update {
            for mapping in &private_mappings {
                if let Some(name) = sley_protocol::refspec_map_source(mapping, destination)? {
                    destinations.push(name);
                }
            }
        }
        for mapping in &tracking_mappings {
            if let Some(name) = sley_protocol::refspec_map_source(mapping, destination)? {
                destinations.push(name);
            }
        }
        destinations.sort();
        destinations.dedup();
        for name in destinations {
            match oid {
                Some(oid) => {
                    let mut transaction = store.transaction();
                    transaction.update(sley_refs::RefUpdate {
                        name,
                        expected: None,
                        new: RefTarget::Direct(oid),
                        reflog: None,
                    });
                    transaction.commit()?;
                }
                None if store.read_ref(&name)?.is_some() => {
                    store.delete_ref(&name)?;
                }
                None => {}
            }
        }
    }
    Ok(())
}

fn restore_helper_import_source_refs(
    git_dir: &Path,
    format: ObjectFormat,
    refs: &[(String, Option<RefTarget>)],
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    for (name, previous) in refs {
        match previous {
            Some(target) => {
                let mut transaction = store.transaction();
                transaction.update(sley_refs::RefUpdate {
                    name: name.clone(),
                    expected: None,
                    new: target.clone(),
                    reflog: None,
                });
                transaction.commit()?;
            }
            None if store.read_ref(name)?.is_some() => {
                store.delete_ref(name)?;
            }
            None => {}
        }
    }
    Ok(())
}

fn run_native_fast_import(git_dir: &Path, stream: &[u8]) -> Result<()> {
    let executable = native_sley_executable()?;
    let mut child = Command::new(&executable)
        .arg("fast-import")
        .arg("--quiet")
        // Remote helpers are allowed to advertise import/export mark paths.
        // Git runs this trusted protocol stream with unsafe features enabled;
        // the path still originates from the explicitly selected user helper.
        .arg("--allow-unsafe-features")
        .env("GIT_DIR", git_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| {
            GitError::Command(format!(
                "could not start native Sley fast-import '{}': {err}",
                PathBuf::from(&executable).display()
            ))
        })?;
    child
        .stdin
        .take()
        .ok_or_else(|| GitError::Command("native fast-import has no stdin".into()))?
        .write_all(stream)?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        eprintln!("error: error while running fast-import");
        Err(GitError::Exit(status.code().unwrap_or(1)))
    }
}

fn native_sley_executable() -> Result<OsString> {
    select_native_sley_executable(std::env::current_exe(), std::env::var_os("SLEY_BIN"))
}

fn select_native_sley_executable(
    current: std::io::Result<PathBuf>,
    _untrusted_sley_bin: Option<OsString>,
) -> Result<OsString> {
    current.map(Into::into).map_err(|error| {
        GitError::Command(format!("cannot resolve current Sley executable: {error}"))
    })
}

fn trace_remote_helper(spec: &sley_remote::RemoteHelperSpec) {
    if !crate::setup::git_trace_enabled() {
        return;
    }
    let mut line = format!(
        "trace: run_command: git remote-{} {}",
        spec.name,
        crate::setup::trace_quote_sq(&spec.alias)
    );
    if let Some(url) = spec.url.as_deref() {
        line.push(' ');
        line.push_str(&crate::setup::trace_quote_sq(url));
    }
    crate::setup::git_trace_line("run-command.c:672", &line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_sley_bin_cannot_redirect_native_plumbing() {
        let current = PathBuf::from("/trusted/current-sley");
        assert_eq!(
            select_native_sley_executable(
                Ok(current.clone()),
                Some(OsString::from("/installed/git")),
            )
            .expect("selection"),
            current.into_os_string()
        );
    }
}
