//! `git fetch-pack` — the fetch plumbing verb (upstream `builtin/fetch-pack.c`).
//!
//! Parses the upstream flag matrix, resolves the destination, matches the
//! sought refs against the remote's advertisement (`filter_refs`), pulls the
//! pack over the in-process local upload-pack, and prints `<oid> <refname>`
//! per fetched ref. `--diag-url` is a faithful port of `connect.c`'s
//! `parse_connect_url` diagnostics and never connects.

use crate::commands::remote::{RemoteCommandContext, ls_remote_git_dir};
use crate::*;
use sley::plumbing::sley_remote::{apply_shallow_info, compute_local_deepen, read_shallow};

const FETCH_PACK_USAGE: &str = "usage: git fetch-pack [--all] [--stdin] [--quiet | -q] [--keep | -k] [--thin] [--include-tag] [--upload-pack=<git-upload-pack>] [--depth=<n>] [--no-progress] [--diag-url] [-v] [<host>:]<directory> [<refs>...]";

/// Upstream `INFINITE_DEPTH` (0x7fffffff).
const INFINITE_DEPTH: u32 = 0x7fff_ffff;

#[derive(Default)]
struct FetchPackFlags {
    fetch_all: bool,
    stdin_refs: bool,
    keep_pack: bool,
    depth: Option<u32>,
    filter: Option<String>,
    diag_url: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchStatus {
    NotMatched,
    Matched,
    UnadvertisedNotAllowed,
}

struct SoughtRef {
    /// Refname, or the full hex oid when fetching a raw object id.
    name: String,
    /// Set when the sought entry parsed as `<oid>` or `<oid> <ref>`.
    oid: Option<ObjectId>,
    status: MatchStatus,
}

/// Upstream `add_sought_entry`: `<oid>`, `<oid> <ref>`, or `<ref>`.
fn parse_sought_entry(format: ObjectFormat, raw: &str) -> SoughtRef {
    let hex_len = format.hex_len();
    if raw.len() >= hex_len
        && let Ok(oid) = ObjectId::from_hex(format, &raw[..hex_len])
    {
        let rest = &raw[hex_len..];
        if rest.is_empty() {
            return SoughtRef {
                name: raw.to_string(),
                oid: Some(oid),
                status: MatchStatus::NotMatched,
            };
        }
        if let Some(name) = rest.strip_prefix(' ') {
            return SoughtRef {
                name: name.to_string(),
                oid: Some(oid),
                status: MatchStatus::NotMatched,
            };
        }
    }
    SoughtRef {
        name: raw.to_string(),
        oid: None,
        status: MatchStatus::NotMatched,
    }
}

pub(crate) fn cmd_fetch_pack(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut flags = FetchPackFlags::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if !arg.starts_with('-') {
            break;
        }
        index += 1;
        match arg {
            "-h" | "--help" => {
                println!("{FETCH_PACK_USAGE}");
                return Err(GitError::Exit(129));
            }
            "--quiet"
            | "-q"
            | "--thin"
            | "--include-tag"
            | "-v"
            | "--no-progress"
            | "--stateless-rpc"
            | "--lock-pack"
            | "--check-self-contained-and-connected"
            | "--cloning"
            | "--update-shallow"
            | "--from-promisor"
            | "--refetch"
            | "--no-filter"
            | "--deepen-relative" => {}
            "--keep" | "-k" => flags.keep_pack = true,
            "--all" => flags.fetch_all = true,
            "--stdin" => flags.stdin_refs = true,
            "--diag-url" => flags.diag_url = true,
            _ => {
                if let Some(value) = arg.strip_prefix("--depth=") {
                    let depth = value.parse::<i64>().unwrap_or(0);
                    flags.depth = Some(depth.clamp(0, i64::from(INFINITE_DEPTH)) as u32);
                } else if let Some(filter) = arg.strip_prefix("--filter=") {
                    flags.filter = Some(filter.to_string());
                } else if arg.starts_with("--upload-pack=")
                    || arg.starts_with("--exec=")
                    || arg.starts_with("--shallow-since=")
                    || arg.starts_with("--shallow-exclude=")
                {
                    // Accepted but unused by the in-process local transport.
                } else {
                    eprintln!("{FETCH_PACK_USAGE}");
                    return Err(GitError::Exit(129));
                }
            }
        }
    }
    let Some(dest) = args.get(index) else {
        eprintln!("{FETCH_PACK_USAGE}");
        return Err(GitError::Exit(129));
    };
    let dest = dest.clone();
    index += 1;

    // Upstream fetch-pack is RUN_SETUP: repository discovery precedes
    // everything, including --diag-url.
    let remote_context = RemoteCommandContext::require_repository(cli_session)?;
    let git_dir = remote_context.required_git_dir()?.to_path_buf();
    let format = repository_object_format(&git_dir)?;

    if flags.diag_url {
        return diag_url(&dest);
    }

    let mut sought: Vec<SoughtRef> = args[index..]
        .iter()
        .map(|raw| parse_sought_entry(format, raw))
        .collect();
    if flags.stdin_refs {
        let mut buffer = String::new();
        io::Read::read_to_string(&mut io::stdin().lock(), &mut buffer)?;
        for line in buffer.lines() {
            if line.is_empty() {
                continue;
            }
            sought.push(parse_sought_entry(format, line));
        }
    }

    let remote_git_dir = ls_remote_git_dir(&remote_context, &dest)?;
    let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
    let remote_format = repository_object_format(&remote_common_git_dir)?;
    if remote_format != format {
        return Err(GitError::InvalidObjectId(format!(
            "remote repository uses {}, local repository uses {}",
            remote_format.name(),
            format.name()
        )));
    }
    let advertisements = sley_remote::local_fetch_advertisements(&remote_git_dir, format)?;
    let remote_config = read_repo_config(&remote_common_git_dir)?;
    let transfer_filter = match flags.filter.as_deref() {
        None => None,
        Some(_)
            if !remote_config
                .get_bool("uploadpack", None, "allowfilter")
                .unwrap_or(false) =>
        {
            eprintln!("warning: filtering not recognized by server, ignoring");
            None
        }
        Some(spec) => Some(
            sley_remote::pack_filter_from_spec(spec)
                .ok_or_else(|| GitError::InvalidFormat(format!("invalid filter-spec '{spec}'")))?,
        ),
    };

    // filter_refs: name matches first, then the raw-oid pass over advertised
    // tips (or the uploadpack.allow*sha1inwant escape hatches).
    let mut sought_by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (position, entry) in sought.iter().enumerate() {
        sought_by_name
            .entry(entry.name.as_str())
            .or_default()
            .push(position);
    }
    let mut matched_positions: Vec<usize> = Vec::new();
    let mut fetched: Vec<(ObjectId, String)> = Vec::new();
    let mut fetched_names: HashSet<String> = HashSet::new();
    for advertisement in &advertisements {
        if advertisement.name.ends_with("^{}") {
            continue;
        }
        let mut keep = false;
        if let Some(positions) = sought_by_name.get(advertisement.name.as_str()) {
            keep = true;
            matched_positions.extend(positions.iter().copied());
        }
        if !keep
            && flags.fetch_all
            && (flags.depth.is_none() || !advertisement.name.starts_with("refs/tags/"))
        {
            keep = true;
        }
        if keep && fetched_names.insert(advertisement.name.clone()) {
            fetched.push((advertisement.oid, advertisement.name.clone()));
        }
    }
    for position in matched_positions {
        sought[position].status = MatchStatus::Matched;
    }

    // Raw-oid pass: a sought entry whose name is a full hex oid matches when
    // that oid is an advertised tip, or when the server allows unadvertised
    // object requests.
    let needs_oid_pass = sought
        .iter()
        .any(|entry| entry.status == MatchStatus::NotMatched && entry.oid.is_some());
    if needs_oid_pass {
        let tip_oids: HashSet<ObjectId> = advertisements
            .iter()
            .map(|advertisement| advertisement.oid)
            .collect();
        let allow_unadvertised = [
            "allowtipsha1inwant",
            "allowreachablesha1inwant",
            "allowanysha1inwant",
        ]
        .iter()
        .any(|key| {
            remote_config
                .get_bool("uploadpack", None, key)
                .unwrap_or(false)
        });
        for entry in &mut sought {
            if entry.status != MatchStatus::NotMatched {
                continue;
            }
            let Some(oid) = entry.oid else { continue };
            if entry.name.len() != format.hex_len() {
                continue;
            }
            if allow_unadvertised || tip_oids.contains(&oid) {
                entry.status = MatchStatus::Matched;
                if fetched_names.insert(entry.name.clone()) {
                    fetched.push((oid, entry.name.clone()));
                }
            } else {
                entry.status = MatchStatus::UnadvertisedNotAllowed;
            }
        }
    }

    let mut wants: Vec<ObjectId> = Vec::new();
    let mut seen_wants: HashSet<ObjectId> = HashSet::new();
    for (oid, _) in &fetched {
        if seen_wants.insert(*oid) {
            wants.push(*oid);
        }
    }

    let remote_db = FileObjectDatabase::from_git_dir(&remote_common_git_dir, format);
    let deepen_plan = match flags.depth {
        Some(depth) if depth > 0 => {
            let client_shallow = read_shallow(&git_dir, format)?;
            Some(compute_local_deepen(
                &remote_db,
                format,
                &wants,
                client_shallow,
                depth,
                false,
            )?)
        }
        _ => None,
    };
    if !wants.is_empty() {
        let shallow_info = sley_remote::install_fetch_pack_via_local_upload_pack(
            &git_dir,
            &remote_git_dir,
            format,
            wants,
            deepen_plan.as_ref(),
            false,
            false,
            transfer_filter,
            None,
            false,
            None,
        )?;
        apply_shallow_info(&git_dir, format, &shallow_info)?;
    }
    let _ = flags.keep_pack; // packs are always installed as packs

    let mut failed = fetched.is_empty();
    // report_unmatched_refs, in sought order.
    for entry in &sought {
        match entry.status {
            MatchStatus::Matched => {}
            MatchStatus::NotMatched => {
                eprintln!("error: no such remote ref {}", entry.name);
                failed = true;
            }
            MatchStatus::UnadvertisedNotAllowed => {
                eprintln!(
                    "error: Server does not allow request for unadvertised object {}",
                    entry.name
                );
                failed = true;
            }
        }
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (oid, name) in &fetched {
        use io::Write as _;
        writeln!(out, "{oid} {name}")?;
    }
    if failed {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// --diag-url: a port of connect.c parse_connect_url + the CONNECT_DIAG_URL
// branches of git_connect.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiagProtocol {
    Local,
    File,
    Ssh,
    Git,
}

impl DiagProtocol {
    fn name(self) -> &'static str {
        match self {
            DiagProtocol::Local | DiagProtocol::File => "file",
            DiagProtocol::Ssh => "ssh",
            DiagProtocol::Git => "git",
        }
    }
}

/// `url.c is_url`: `[A-Za-z0-9][A-Za-z0-9+.-]*` followed by `://`.
fn is_url(url: &str) -> bool {
    let bytes = url.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b':' {
            break;
        }
        let alphanumeric = byte.is_ascii_alphanumeric();
        let special = byte == b'+' || byte == b'-' || byte == b'.';
        if index == 0 {
            if !alphanumeric {
                return false;
            }
        } else if !alphanumeric && !special {
            return false;
        }
        index += 1;
    }
    index > 0 && bytes[index..].starts_with(b"://")
}

/// `url.c url_decode`: percent-decode everything after the scheme colon.
fn url_decode(url: &str) -> String {
    let bytes = url.as_bytes();
    let start = url.find(':').map(|p| p + 1).unwrap_or(0);
    let mut out = String::with_capacity(url.len());
    out.push_str(&url[..start]);
    let mut index = start;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (
                (bytes[index + 1] as char).to_digit(16),
                (bytes[index + 2] as char).to_digit(16),
            )
        {
            out.push(((high * 16 + low) as u8) as char);
            index += 3;
            continue;
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}

/// `connect.c url_is_local_not_ssh`.
fn url_is_local_not_ssh(url: &str) -> bool {
    let colon = url.find(':');
    let slash = url.find('/');
    match colon {
        None => true,
        Some(colon_pos) => slash.is_some_and(|slash_pos| slash_pos < colon_pos),
    }
}

/// `connect.c get_protocol` — dies on unknown schemes.
fn get_protocol(name: &str) -> Result<DiagProtocol> {
    match name {
        "ssh" | "git+ssh" | "ssh+git" => Ok(DiagProtocol::Ssh),
        "git" => Ok(DiagProtocol::Git),
        "file" => Ok(DiagProtocol::File),
        _ => {
            eprintln!("fatal: protocol '{name}' is not supported");
            Err(GitError::Exit(128))
        }
    }
}

/// `connect.c host_end` (removebrackets = false): the index just past a
/// leading `[...]` group (host start when there is none). The bracket group
/// may follow a `user@` prefix (`@[`).
fn host_end_index(host: &str) -> usize {
    let start = match host.find("@[") {
        Some(position) => position + 1,
        None => 0,
    };
    if host[start..].starts_with('[') {
        match host[start + 1..].find(']') {
            // C keeps `end` AT the ']' without removebrackets.
            Some(close) => start + 1 + close,
            None => 0,
        }
    } else {
        0
    }
}

/// `connect.c host_end` with removebrackets: strip one `[...]` pair, returning
/// the rewritten host and the index just past the bracket content.
fn strip_host_brackets(host: &str) -> (String, usize) {
    let start = match host.find("@[") {
        Some(position) => position + 1,
        None => 0,
    };
    if host[start..].starts_with('[')
        && let Some(close) = host[start + 1..].find(']')
    {
        let inner_end = start + 1 + close;
        let mut rewritten = String::with_capacity(host.len() - 2);
        rewritten.push_str(&host[..start]);
        rewritten.push_str(&host[start + 1..inner_end]);
        rewritten.push_str(&host[inner_end + 1..]);
        return (rewritten, inner_end - 1);
    }
    (host.to_string(), 0)
}

/// Is `value` a valid port number per `strtol`-then-range check?
fn parse_port(value: &str) -> Option<&str> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number: i64 = value.parse().ok()?;
    if (0..65536).contains(&number) {
        Some(value)
    } else {
        None
    }
}

/// `connect.c get_host_and_port`: split a trailing `:port`, looking only past
/// any bracket group (which is stripped). Returns (host, port).
fn get_host_and_port(host: &str) -> (String, Option<String>) {
    let (mut stripped, end) = strip_host_brackets(host);
    if let Some(colon_rel) = stripped[end..].find(':') {
        let colon = end + colon_rel;
        let after = stripped[colon + 1..].to_string();
        if let Some(port) = parse_port(&after) {
            let port = port.to_string();
            stripped.truncate(colon);
            return (stripped, Some(port));
        }
        if after.is_empty() {
            stripped.truncate(colon);
        }
    }
    (stripped, None)
}

/// `connect.c get_port`: a trailing all-numeric `:port` anywhere in the host.
fn split_get_port(host: &str) -> (String, Option<String>) {
    if let Some(colon) = host.find(':') {
        let after = &host[colon + 1..];
        if parse_port(after).is_some() {
            return (host[..colon].to_string(), Some(after.to_string()));
        }
    }
    (host.to_string(), None)
}

fn no_path_specified() -> GitError {
    eprintln!("fatal: no path specified; see 'git help pull' for valid url syntax");
    GitError::Exit(128)
}

struct ParsedConnectUrl {
    protocol: DiagProtocol,
    hostandport: String,
    path: String,
}

/// `connect.c parse_connect_url`.
fn parse_connect_url(url_orig: &str) -> Result<ParsedConnectUrl> {
    let decoded = if is_url(url_orig) {
        url_decode(url_orig)
    } else {
        url_orig.to_string()
    };

    let mut separator = '/';
    let (protocol, host): (DiagProtocol, &str) = match decoded.find("://") {
        Some(scheme_end) => (
            get_protocol(&decoded[..scheme_end])?,
            &decoded[scheme_end + 3..],
        ),
        None => {
            if url_is_local_not_ssh(&decoded) {
                (DiagProtocol::Local, decoded.as_str())
            } else {
                separator = ':';
                (DiagProtocol::Ssh, decoded.as_str())
            }
        }
    };

    let end = host_end_index(host);

    // Path discovery. (The PROTO_FILE dos-drive / offset_1st_component
    // special cases are Windows-only; on POSIX both reduce to the generic
    // separator search.)
    let path_index = match protocol {
        DiagProtocol::Local => Some(0),
        _ => host[end..].find(separator).map(|p| end + p),
    };
    let Some(path_index) = path_index else {
        return Err(no_path_specified());
    };
    if host[path_index..].is_empty() {
        return Err(no_path_specified());
    }

    let hostandport = host[..path_index].to_string();
    let mut path = &host[path_index..];
    if separator == ':' {
        path = &path[1..];
    }
    if matches!(protocol, DiagProtocol::Git | DiagProtocol::Ssh)
        && path.len() >= 2
        && path.as_bytes()[1] == b'~'
    {
        path = &path[1..];
    }
    if path.is_empty() {
        return Err(no_path_specified());
    }
    Ok(ParsedConnectUrl {
        protocol,
        hostandport,
        path: path.to_string(),
    })
}

/// The `CONNECT_DIAG_URL` output of `git_connect`, for both the non-ssh and
/// ssh branches. Always exits 0 (upstream returns NULL → `args.diag_url ? 0 : 1`).
fn diag_url(url: &str) -> Result<()> {
    let parsed = parse_connect_url(url)?;
    if parsed.protocol != DiagProtocol::Ssh {
        println!("Diag: url={url}");
        println!("Diag: protocol={}", parsed.protocol.name());
        println!("Diag: hostandport={}", parsed.hostandport);
        println!("Diag: path={}", parsed.path);
        return Ok(());
    }
    let (host, mut port) = get_host_and_port(&parsed.hostandport);
    let host = if port.is_none() {
        let (host, found) = split_get_port(&host);
        port = found;
        host
    } else {
        host
    };
    println!("Diag: url={url}");
    println!("Diag: protocol={}", parsed.protocol.name());
    println!("Diag: userandhost={host}");
    println!("Diag: port={}", port.as_deref().unwrap_or("NONE"));
    println!("Diag: path={}", parsed.path);
    Ok(())
}
