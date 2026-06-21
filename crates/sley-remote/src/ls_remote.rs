//! Callable ls-remote advertisement listing for HTTP(S) and local remotes.
//!
//! [`ls_remote`] returns the advertised refs a `git ls-remote` would print for a
//! resolved remote, as a [`LsRemoteRecord`] list, without sorting, printing, or
//! exit-code mapping — those stay in the CLI (the `--sort`/`--symref` formatting
//! and the `--exit-code` ⇒ exit-2 behavior are CLI concerns). Everything is taken
//! as explicit parameters — the resolved [`LsRemoteSource`], the request
//! [`ObjectFormat`], a [`LsRemoteFilter`], a ref-name match predicate, and a
//! [`CredentialProvider`] — so it never reads process-global state, parses
//! arguments, or prints.
//!
//! The ref-name glob/pattern matching (`refs/heads/*` style filters, peeled-tag
//! `^{}` matching) is the CLI's larger ref-filter machinery, so it is injected as
//! the `matches` predicate rather than moved; this module only applies the
//! ref-class filters (`--heads`/`--tags`/`--refs`) and shapes the records.
//!
//! SSH ls-remote still lives in the CLI; only HTTP and local move here.

#[cfg(feature = "http")]
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::ObjectType;
use sley_odb::{FileObjectDatabase, ObjectReader};
use sley_refs::{FileRefStore, Ref, RefTarget};
use sley_transport::RemoteUrl;

use crate::CredentialProvider;

/// How [`ls_remote`] obtains the ref advertisements.
///
/// The caller resolves the remote (URL rewriting, repository discovery — all
/// process-state dependent) and hands `ls_remote` a concrete transport.
pub enum LsRemoteSource {
    /// A smart-HTTP(S) remote at the given already-resolved URL.
    Http(RemoteUrl),
    /// An SSH remote at the given already-resolved URL, listed by spawning `ssh`
    /// (the credential seam is unused — the `ssh` program owns authentication).
    Ssh(RemoteUrl),
    /// A native anonymous `git://` remote at the given already-resolved URL.
    Git(RemoteUrl),
    /// A local repository read directly from `git_dir` (refs and the object
    /// database used to peel annotated tags both resolve from this `$GIT_DIR`,
    /// matching `git ls-remote` against a local path).
    Local {
        /// The remote repository's `$GIT_DIR`.
        git_dir: PathBuf,
    },
}

/// The ref-class filters that select which advertised refs to keep, mirroring the
/// `git ls-remote` flags the CLI parses.
#[derive(Debug, Clone, Copy, Default)]
pub struct LsRemoteFilter {
    /// Limit to branch refs (`--heads`/`--branches`).
    pub heads: bool,
    /// Limit to tag refs (`--tags`).
    pub tags: bool,
    /// Drop `HEAD` and peeled `^{}` entries (`--refs`).
    pub refs_only: bool,
}

/// One advertised ref returned by [`ls_remote`] — what the CLI prints as a
/// `<oid>\t<name>` line (with an optional preceding `ref: <symref>\t<name>` line
/// when `--symref` is set and `symref` is present).
#[derive(Debug, Clone)]
pub struct LsRemoteRecord {
    /// The object id the ref points at (peeled to the tag object for `^{}`
    /// records).
    pub oid: ObjectId,
    /// The full ref name (e.g. `refs/heads/main`, `HEAD`, or `refs/tags/v1^{}`).
    pub name: String,
    /// The symref target, when the remote advertised this ref as a symbolic ref
    /// (e.g. `HEAD` → `refs/heads/main`).
    pub symref: Option<String>,
}

/// List the advertised refs for a resolved `source`.
///
/// Performs the work the CLI's `ls_remote_http_records` and inline local
/// ls-remote path did: advertises the remote's refs (HTTP) or reads them directly
/// (local), applies the `--heads`/`--tags`/`--refs` class filters and the
/// caller-supplied `matches` ref-name predicate, and shapes the surviving refs
/// into [`LsRemoteRecord`]s. For the local path it also emits peeled `^{}` records
/// for annotated tags (unless `refs_only`).
///
/// `format` is the request/expected object format (SHA-1 for HTTP, the local
/// repository's format for local); the returned [`ObjectFormat`] is the format
/// actually in effect (HTTP resolves it from the advertisement). Returns the
/// records and that format; never sorts, prints, or returns `GitError::Exit`. The
/// caller applies `--sort`, `--symref` formatting, and the `--exit-code` mapping.
pub fn ls_remote(
    source: &LsRemoteSource,
    format: ObjectFormat,
    filter: &LsRemoteFilter,
    matches: &dyn Fn(&str) -> bool,
    config: Option<&GitConfig>,
    #[cfg_attr(not(feature = "http"), allow(unused_variables))]
    credentials: &mut dyn CredentialProvider,
) -> Result<(Vec<LsRemoteRecord>, ObjectFormat)> {
    crate::protocol::check_transport_allowed(scheme_for_ls_remote_source(source), config, None)
        .map_err(crate::protocol::transport_policy_git_error)?;
    match source {
        #[cfg(feature = "http")]
        LsRemoteSource::Http(remote) => {
            ls_remote_http(remote, format, filter, matches, credentials)
        }
        #[cfg(not(feature = "http"))]
        LsRemoteSource::Http(_) => Err(GitError::Unsupported(
            "HTTP transport is not enabled in this build".into(),
        )),
        LsRemoteSource::Ssh(remote) => crate::ssh::ls_remote_ssh(remote, filter, matches),
        LsRemoteSource::Git(remote) => crate::git::ls_remote_git(
            remote,
            filter,
            matches,
            config
                .and_then(|config| config.get("protocol", None, "version"))
                == Some("2"),
        ),
        LsRemoteSource::Local { git_dir } => ls_remote_local(git_dir, format, filter, matches),
    }
}

fn scheme_for_ls_remote_source(source: &LsRemoteSource) -> &'static str {
    match source {
        LsRemoteSource::Http(remote) => crate::protocol::transport_scheme_for_remote(remote),
        LsRemoteSource::Ssh(remote) => crate::protocol::transport_scheme_for_remote(remote),
        LsRemoteSource::Git(remote) => crate::protocol::transport_scheme_for_remote(remote),
        LsRemoteSource::Local { .. } => "file",
    }
}

/// List advertised refs over smart HTTP(S): fetch the upload-pack advertisement,
/// then apply the class filters and `matches` predicate, attaching the advertised
/// `HEAD` symref where present.
#[cfg(feature = "http")]
fn ls_remote_http(
    remote: &RemoteUrl,
    format: ObjectFormat,
    filter: &LsRemoteFilter,
    matches: &dyn Fn(&str) -> bool,
    credentials: &mut dyn CredentialProvider,
) -> Result<(Vec<LsRemoteRecord>, ObjectFormat)> {
    let client = crate::http::new_http_client();
    let (refs, features) =
        crate::http::http_upload_pack_advertisements(&client, remote, format, credentials)?;
    let format = features.object_format.unwrap_or(ObjectFormat::Sha1);
    if format != ObjectFormat::Sha1 {
        return Err(GitError::Unsupported(format!(
            "http ls-remote currently supports SHA-1 advertisements, got {}",
            format.name()
        )));
    }
    let symrefs = features
        .symrefs
        .iter()
        .filter_map(|symref| symref.split_once(':'))
        .map(|(name, target)| (name.to_string(), target.to_string()))
        .collect::<HashMap<_, _>>();
    let mut records = Vec::new();
    for advertisement in refs {
        if advertisement.oid.is_null() {
            continue;
        }
        if filter.refs_only && (advertisement.name == "HEAD" || advertisement.name.ends_with("^{}"))
        {
            continue;
        }
        if !ref_class_selected(&advertisement.name, filter) {
            continue;
        }
        if !matches(&advertisement.name) {
            continue;
        }
        records.push(LsRemoteRecord {
            oid: advertisement.oid,
            symref: symrefs.get(&advertisement.name).cloned(),
            name: advertisement.name,
        });
    }
    Ok((records, format))
}

/// List advertised refs from a local repository at `git_dir`: `HEAD` (when no
/// class filter is active), then every ref resolved to its object id, plus a
/// peeled `^{}` record for each annotated tag (unless `refs_only`).
fn ls_remote_local(
    git_dir: &Path,
    format: ObjectFormat,
    filter: &LsRemoteFilter,
    matches: &dyn Fn(&str) -> bool,
) -> Result<(Vec<LsRemoteRecord>, ObjectFormat)> {
    let store = FileRefStore::new(git_dir, format);
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut records = Vec::new();

    if !filter.refs_only
        && !filter.heads
        && !filter.tags
        && let Some(target) = store.read_ref("HEAD")?
    {
        let reference = Ref {
            name: "HEAD".to_string(),
            target,
        };
        if matches(&reference.name)
            && let Some((oid, symref)) = resolve_for_each_ref_target(&store, &reference)?
        {
            records.push(LsRemoteRecord {
                oid,
                name: reference.name,
                symref,
            });
        }
    }

    for reference in store.list_refs()? {
        if !ref_class_selected(&reference.name, filter) {
            continue;
        }
        if !matches(&reference.name) {
            continue;
        }
        let Some((oid, symref)) = resolve_for_each_ref_target(&store, &reference)? else {
            continue;
        };
        records.push(LsRemoteRecord {
            oid,
            name: reference.name.clone(),
            symref,
        });
        if !filter.refs_only
            && let Some(record) = peeled_tag_record(&db, format, &oid, &reference.name, matches)?
        {
            records.push(record);
        }
    }

    Ok((records, format))
}

/// The peeled `^{}` record for `name` when `oid` is an annotated tag and the
/// peeled name passes `matches`; `None` otherwise.
fn peeled_tag_record(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    name: &str,
    matches: &dyn Fn(&str) -> bool,
) -> Result<Option<LsRemoteRecord>> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Tag {
        return Ok(None);
    }
    let peeled_name = format!("{name}^{{}}");
    if !matches(&peeled_name) {
        return Ok(None);
    }
    let peeled = sley_rev::peel_tags(db, format, oid)?;
    Ok(Some(LsRemoteRecord {
        oid: peeled,
        name: peeled_name,
        symref: None,
    }))
}

/// Whether `name` survives the `--heads`/`--tags` class filter (no class filter
/// keeps everything; with one or both set, the ref must be in a selected class).
fn ref_class_selected(name: &str, filter: &LsRemoteFilter) -> bool {
    if !filter.heads && !filter.tags {
        return true;
    }
    let is_head = name.starts_with("refs/heads/");
    let is_tag = name.starts_with("refs/tags/");
    (filter.heads && is_head) || (filter.tags && is_tag)
}

/// Resolve a (possibly symbolic) ref target to its object id, following up to
/// five levels of symbolic indirection, returning the first symbolic name seen.
fn resolve_for_each_ref_target(
    store: &FileRefStore,
    reference: &Ref,
) -> Result<Option<(ObjectId, Option<String>)>> {
    let mut target = reference.target.clone();
    let mut symref = None;
    for _ in 0..5 {
        match target {
            RefTarget::Direct(oid) => return Ok(Some((oid, symref))),
            RefTarget::Symbolic(name) => {
                symref.get_or_insert_with(|| name.clone());
                let Some(next) = store.read_ref(&name)? else {
                    return Ok(None);
                };
                target = next;
            }
        }
    }
    Ok(None)
}
