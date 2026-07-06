//! `git ls-remote` command and formatting.
#![allow(clippy::expect_used)]

#[path = "ls_remote_options.rs"]
mod ls_remote_options;
use ls_remote_options::setup_ls_remote_options;
use sley::plumbing::{sley_config};

use super::config::{read_repo_config, remote_exists};
use super::fetch::{
    check_transport_allowed_url, configured_server_options, default_fetch_remote,
    ls_remote_resolved_url, repo_config_with_transport_policy, transport_policy_config_for_cwd,
};
use super::pack::{
    configured_legacy_protocol, configured_protocol_version, trace_configured_local_protocol_version,
    trace_protocol_v2_ls_refs_request, trace_protocol_v2_upload_pack_capabilities,
};
use super::resolve::ls_remote_git_dir;
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
    let config = crate::session::cli_git_dir()
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
pub(super) struct LsRemoteOptions {
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
pub(super) enum LsRemoteSort {
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
    let Ok(git_dir) = crate::session::cli_git_dir_from(&cwd) else {
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
    let git_dir = crate::session::cli_git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    default_fetch_remote(&git_dir, format)
}

pub(crate) fn cmd_ls_remote(args: &[String]) -> Result<()> {
    let mut options = setup_ls_remote_options(args)?;
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
        let mut records = ls_remote_upload_pack_command_records(command, &repository, &options)?;
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
    repository: &str,
    options: &LsRemoteOptions,
) -> Result<Vec<LsRemoteRecord>> {
    let command = format!("{command} {}", sley_config::sq_quote(repository));
    let output = Proc::new("/bin/sh")
        .arg("-c")
        .arg(&command)
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
    if let Ok(git_dir) = crate::session::cli_git_dir() {
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

fn ls_remote_display_url(repository: &str) -> Result<String> {
    let cwd = env::current_dir()?;
    let config = crate::session::cli_git_dir_from(&cwd)
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
