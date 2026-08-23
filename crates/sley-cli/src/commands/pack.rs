//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used)]

use sley::plumbing::sley_core;
// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::commands::cli_options::{cli_usage_error, last_tri_state_bool, opt_bool, opt_str};
use crate::*;
use sley::plumbing::sley_formats::ReftableWriteOptions;
use sley::plumbing::sley_object::EncodedObject;
use sley::plumbing::sley_odb::ObjectReader;
use sley::plumbing::sley_pack::{PackReverseIndex, pack_order_index_positions};
use sley_options::{OptFlags, OptionName, ParsedValue, parse_options};
use std::sync::Arc;

#[derive(Debug)]
struct IndexPackOptions {
    verbose: bool,
    output: Option<PathBuf>,
    keep: bool,
    /// Explicit `--[no-]rev-index`; when absent, `pack.writeReverseIndex`
    /// decides whether to write the sidecar (and defaults to false for
    /// index-pack, matching upstream).
    rev_index: Option<bool>,
    verify: bool,
    /// Internal statistics-only verification used by verify-pack and pack
    /// depth diagnostics (`index-pack --verify-stat-only <pack>`).
    verify_stat_only: bool,
    stdin: bool,
    fix_thin: bool,
    pack_file: Option<PathBuf>,
    /// `--index-version=<n>` (or `<n>,<offset-threshold>`); `1` writes a v1
    /// `.idx`, anything else keeps the v2 default.
    index_version: Option<u32>,
    /// `--strict` / `--fsck-objects`: fsck every packed object and report
    /// content findings. The optional `=<msg-id>=<severity>...` suffix carries
    /// severity overrides (e.g. `missingEmail=ignore`).
    fsck: bool,
    /// `--strict` additionally rejects duplicate object ids in the incoming
    /// pack. Ordinary index-pack accepts and indexes every duplicate copy.
    strict: bool,
    /// Raw `<msg-id>=<severity>` override tokens from `--strict=`/`--fsck-objects=`.
    fsck_overrides: Vec<String>,
    /// `--object-format=<algo>`: the hash algorithm. Lets `index-pack <pack>`
    /// run outside a repository (where there is no config to read it from).
    object_format: Option<ObjectFormat>,
    /// `--max-input-size=<n>`: reject a pack whose byte length exceeds `<n>`.
    max_input_size: Option<u64>,
}

pub(crate) fn cmd_index_pack(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let options = setup_index_pack_options(args)?;
    // The hash algorithm is taken from `--object-format` when given, else from
    // the surrounding repository. A `<pack-file>` argument (not `--stdin`) can
    // run outside any repo, so only fall back to repo discovery when needed.
    let repo = match cli_session.git_dir() {
        Ok(git_dir) => match common_git_dir_for_git_dir(&git_dir) {
            Ok(common_git_dir) => {
                let format = match options.object_format {
                    Some(format) => format,
                    None => repository_object_format(&common_git_dir)?,
                };
                Some((common_git_dir, format))
            }
            Err(_err) if !options.stdin && options.object_format.is_some() => None,
            Err(err) => return Err(err),
        },
        Err(err) => {
            // Outside a repo, a file-mode index-pack is still valid: `git`
            // falls back to the built-in hash (SHA-1) when no repository names
            // one, and `--object-format` can override it. Only `--stdin`, which
            // installs into the object store, genuinely needs the repository.
            if options.stdin {
                return Err(err);
            }
            None
        }
    };
    let format = options
        .object_format
        .or_else(|| repo.as_ref().map(|(_, format)| *format))
        .unwrap_or(ObjectFormat::Sha1);
    if options.stdin {
        let (common_git_dir, _) = repo
            .as_ref()
            .expect("stdin index-pack requires a repository");
        let common_git_dir = common_git_dir.clone();
        let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
        let install = if options.verbose || options.fsck || options.fix_thin {
            let mut pack = Vec::new();
            io::stdin().read_to_end(&mut pack)?;
            // `index-pack -v` reports two phases on stderr: receiving the pack and
            // resolving deltas (builtin/index-pack.c start_progress messages). The
            // object count is the pack header's 32-bit big-endian field at bytes
            // 8..12.
            if options.verbose && pack.len() >= 12 {
                let count = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]);
                eprintln!("Receiving objects: 100% ({count}/{count}), done.");
                eprintln!("Resolving deltas: 100% ({count}/{count}), done.");
                sley_core::trace2::region("progress", "index-pack");
            }
            if options.strict && pack_has_duplicate_objects(&pack, format)? {
                return Err(GitError::Exit(1));
            }
            if options.fsck {
                let exit = fsck_pack_objects(&pack, format, &options.fsck_overrides)?;
                if exit != 0 {
                    return Err(GitError::Exit(exit));
                }
            }
            let mut reader = pack.as_slice();
            if options.fix_thin {
                db.install_raw_pack_from_reader_with_external_bases(&mut reader)?
            } else {
                db.install_raw_pack_from_reader(&mut reader)?
            }
        } else {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            db.install_raw_pack_from_reader(&mut stdin)?
        };
        if options.keep {
            let keep_path = install.pack_path.with_extension("keep");
            fs::write(keep_path, b"")?;
        }
        if effective_index_pack_reverse_index(&options, repo.as_ref().map(|(dir, _)| dir))? {
            let index = PackIndex::parse(&fs::read(&install.index_path)?, format)?;
            write_reverse_index_for_entries(
                &install.pack_path.with_extension("rev"),
                format,
                &index.entries,
                &index.pack_checksum,
            )?;
        }
        println!("pack\t{}", install.pack_name.trim_start_matches("pack-"));
        return Ok(());
    }

    let Some(pack_file) = options.pack_file.clone() else {
        return index_pack_usage();
    };
    let pack = fs::read(&pack_file)?;
    if options.verify_stat_only {
        return verify_pack_one(format, &pack_file, false, true);
    }
    // `--max-input-size`: refuse a pack larger than the cap, mirroring
    // index-pack.c's `pack exceeds maximum allowed size` die.
    if let Some(limit) = options.max_input_size
        && pack.len() as u64 > limit
    {
        eprintln!(
            "fatal: pack exceeds maximum allowed size ({})",
            humanise_byte_count(limit)
        );
        return Err(GitError::Exit(128));
    }
    let indexed = PackFile::index_pack(&pack, format)?;
    // `--strict` / `--fsck-objects`: fsck every object the pack carries and
    // report content findings, mirroring index-pack.c's fsck_finish pass.
    if options.fsck {
        // A reference to an object not present in the pack queues a connectivity
        // check that must be resolved against the surrounding object store; git
        // refuses to run those queued checks with no repository. A self-contained
        // pack (every link resolvable in-pack) fscks fine outside a repo.
        if repo.is_none() && pack_has_unresolved_link(&pack, format)? {
            eprintln!("fatal: cannot perform queued object checks outside of a repository");
            return Err(GitError::Exit(128));
        }
        let exit = fsck_pack_objects(&pack, format, &options.fsck_overrides)?;
        if exit != 0 {
            return Err(GitError::Exit(exit));
        }
    }
    let write_reverse_index =
        effective_index_pack_reverse_index(&options, repo.as_ref().map(|(dir, _)| dir))?;
    if options.verify {
        if write_reverse_index {
            let rev_path = pack_file.with_extension("rev");
            let reverse = fs::read(&rev_path).map_err(|err| {
                GitError::InvalidFormat(format!(
                    "reverse-index validation error for {}: {err}",
                    rev_path.display()
                ))
            })?;
            PackReverseIndex::parse(&reverse, format, indexed.entries.len()).map_err(|err| {
                GitError::InvalidFormat(format!(
                    "reverse-index validation error for {}: {err}",
                    rev_path.display()
                ))
            })?;
        }
        return Ok(());
    }
    let index_path = options
        .output
        .unwrap_or_else(|| pack_file.with_extension("idx"));
    // `--index-version=1` re-serialises the same entries in the v1 layout; the
    // default (and `=2`) keeps the v2 index `index_pack` already produced.
    let index_bytes = if options.index_version == Some(1) {
        PackIndex::write_v1(format, &indexed.entries, &indexed.checksum)?
    } else {
        indexed.index.clone()
    };
    write_index_pack_output(&index_path, &index_bytes)?;
    if options.keep {
        fs::write(pack_file.with_extension("keep"), b"")?;
    }
    if write_reverse_index {
        write_reverse_index_for_entries(
            &index_path.with_extension("rev"),
            format,
            &indexed.entries,
            &indexed.checksum,
        )?;
    }
    if options.verbose {
        eprintln!(
            "Indexing objects: 100% ({}/{})",
            indexed.entries.len(),
            indexed.entries.len()
        );
    }
    println!("{}", indexed.checksum);
    Ok(())
}

fn setup_index_pack_options(args: &[String]) -> Result<IndexPackOptions> {
    let parsed = parse_options(args, index_pack_option_specs(), INDEX_PACK_USAGE)
        .map_err(cli_usage_error)?;
    let mut options = IndexPackOptions {
        verbose: false,
        output: None,
        keep: false,
        rev_index: None,
        verify: false,
        verify_stat_only: false,
        stdin: false,
        fix_thin: false,
        pack_file: None,
        index_version: None,
        fsck: false,
        strict: false,
        fsck_overrides: Vec::new(),
        object_format: None,
        max_input_size: None,
    };
    if parsed
        .options
        .iter()
        .any(|option| option.short == Some('v'))
    {
        options.verbose = true;
    }
    for option in &parsed.options {
        match (option.short, option.long) {
            (Some('o'), _) => {
                if let ParsedValue::Str(value) = &option.value {
                    options.output = Some(PathBuf::from(value.to_string()));
                }
            }
            (_, Some("stdin")) => options.stdin = true,
            (_, Some("fix-thin")) => options.fix_thin = true,
            (_, Some("keep")) if !matches!(option.name, OptionName::NegatedLong("keep")) => {
                options.keep = true;
            }
            (_, Some("rev-index")) => {
                options.rev_index =
                    Some(!matches!(option.name, OptionName::NegatedLong("rev-index")));
            }
            (_, Some("verify")) => options.verify = true,
            (_, Some("verify-stat-only")) => options.verify_stat_only = true,
            (_, Some("index-version")) => {
                if let ParsedValue::Str(spec) = &option.value {
                    let version_part = spec.split(',').next().unwrap_or(spec);
                    let version: u32 = version_part
                        .parse()
                        .map_err(|_| GitError::Command(format!("bad index version '{spec}'")))?;
                    if version != 1 && version != 2 {
                        eprintln!("fatal: bad index version '{spec}'");
                        return Err(GitError::Exit(128));
                    }
                    options.index_version = Some(version);
                }
            }
            (_, Some("strict")) | (_, Some("fsck-objects")) => {
                options.fsck = true;
                options.strict |= option.long == Some("strict");
                if let ParsedValue::Str(spec) = &option.value {
                    for token in spec.split(',') {
                        if !token.is_empty() {
                            options.fsck_overrides.push(token.to_string());
                        }
                    }
                }
            }
            (_, Some("object-format")) => {
                if let ParsedValue::Str(value) = &option.value {
                    options.object_format = Some(parse_verify_pack_object_format(value)?);
                }
            }
            (_, Some("max-input-size")) => {
                if let ParsedValue::Str(spec) = &option.value {
                    options.max_input_size =
                        Some(spec.parse().map_err(|_| {
                            GitError::Command(format!("bad max-input-size '{spec}'"))
                        })?);
                }
            }
            _ => {}
        }
    }
    for positional in parsed.positionals {
        index_pack_add_pack_file(&mut options, positional)?;
    }
    if options.output.is_some() && options.verify {
        return Err(GitError::Exit(128));
    }
    if !options.stdin && options.pack_file.is_none() {
        return index_pack_usage();
    }
    Ok(options)
}

fn pack_has_duplicate_objects(pack: &[u8], format: ObjectFormat) -> Result<bool> {
    let parsed = PackFile::parse(pack, format)?;
    let mut seen = HashSet::with_capacity(parsed.entries.len());
    Ok(parsed
        .entries
        .iter()
        .any(|entry| !seen.insert(entry.entry.oid)))
}

fn effective_index_pack_reverse_index(
    options: &IndexPackOptions,
    common_git_dir: Option<&PathBuf>,
) -> Result<bool> {
    if let Some(explicit) = options.rev_index {
        return Ok(explicit);
    }
    let Some(common_git_dir) = common_git_dir else {
        return Ok(false);
    };
    Ok(read_repo_config(common_git_dir)?
        .get_bool("pack", None, "writeReverseIndex")
        .unwrap_or(false))
}

fn write_reverse_index_for_entries(
    path: &Path,
    format: ObjectFormat,
    entries: &[sley_pack::PackIndexEntry],
    pack_checksum: &ObjectId,
) -> Result<()> {
    let positions = pack_order_index_positions(entries);
    let bytes = PackReverseIndex::write(format, &positions, pack_checksum)?;
    fs::write(path, bytes)?;
    Ok(())
}

const INDEX_PACK_USAGE: &[&str] = &[
    "git index-pack [-v] [-o <index-file>] [--keep | --keep=<msg>] [--[no-]rev-index] [--verify] [--strict[=<msg-id>=<severity>...]] [--fsck-objects[=<msg-id>=<severity>...]] (<pack-file> | --stdin [--fix-thin] [<pack-file>])",
];

fn index_pack_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[
        opt_bool(Some('v'), None, OptFlags::NONEG, "verbose"),
        opt_str(
            Some('o'),
            None,
            "<index-file>",
            OptFlags::NONE,
            "write index to <index-file>",
        ),
        opt_bool(None, Some("stdin"), OptFlags::NONEG, "read from stdin"),
        opt_bool(None, Some("fix-thin"), OptFlags::NONEG, "fix thin pack"),
        opt_str(
            None,
            Some("keep"),
            "<msg>",
            OptFlags::OPTARG.union(OptFlags::NONEG),
            "create .keep file",
        ),
        opt_bool(
            None,
            Some("rev-index"),
            OptFlags::NONE,
            "generate reverse index",
        ),
        opt_bool(None, Some("verify"), OptFlags::NONEG, "verify pack"),
        opt_bool(
            None,
            Some("verify-stat-only"),
            OptFlags::HIDDEN.union(OptFlags::NONEG),
            "show pack statistics only",
        ),
        opt_str(
            None,
            Some("index-version"),
            "<version>",
            OptFlags::NONEG,
            "index version",
        ),
        opt_str(
            None,
            Some("strict"),
            "<msg-id>=<severity>...",
            OptFlags::OPTARG.union(OptFlags::NONEG),
            "fsck objects",
        ),
        opt_str(
            None,
            Some("fsck-objects"),
            "<msg-id>=<severity>...",
            OptFlags::OPTARG.union(OptFlags::NONEG),
            "fsck objects",
        ),
        opt_str(
            None,
            Some("object-format"),
            "<format>",
            OptFlags::NONE,
            "specify the hash algorithm to use",
        ),
        opt_str(
            None,
            Some("max-input-size"),
            "<n>",
            OptFlags::NONEG,
            "maximum input size",
        ),
        opt_str(
            None,
            Some("threads"),
            "<n>",
            OptFlags::OPTARG.union(OptFlags::HIDDEN),
            "",
        ),
        opt_str(
            None,
            Some("pack_header"),
            "<hdr>",
            OptFlags::OPTARG.union(OptFlags::HIDDEN),
            "",
        ),
    ];
    SPECS
}

fn index_pack_add_pack_file(options: &mut IndexPackOptions, value: &str) -> Result<()> {
    if options.pack_file.is_some() {
        return index_pack_usage();
    }
    options.pack_file = Some(PathBuf::from(value));
    Ok(())
}

/// Render `bytes` the way git's `strbuf_humanise_bytes` does: `<n> byte[s]`
/// under 1 KiB, otherwise `<x>.<yy> KiB/MiB/GiB` with truncating fixed-point.
pub(crate) fn humanise_byte_count(bytes: u64) -> String {
    if bytes > 1 << 30 {
        let whole = bytes >> 30;
        let frac = (bytes & ((1 << 30) - 1)) / 10_737_419;
        format!("{whole}.{frac:02} GiB")
    } else if bytes > 1 << 20 {
        let x = bytes + 5243;
        let whole = x >> 20;
        let frac = ((x & ((1 << 20) - 1)) * 100) >> 20;
        format!("{whole}.{frac:02} MiB")
    } else if bytes > 1 << 10 {
        let x = bytes + 5;
        let whole = x >> 10;
        let frac = ((x & ((1 << 10) - 1)) * 100) >> 10;
        format!("{whole}.{frac:02} KiB")
    } else if bytes == 1 {
        "1 byte".to_string()
    } else {
        format!("{bytes} bytes")
    }
}

/// True when some object in the pack references an object that is not itself in
/// the pack (a dangling tree/commit/tag link). git queues such links for a
/// connectivity check that can only run against a repository's object store, so
/// `index-pack --fsck-objects` fails outside a repo when this returns true.
fn pack_has_unresolved_link(pack_bytes: &[u8], format: ObjectFormat) -> Result<bool> {
    let pack = match sley_pack::PackFile::parse(pack_bytes, format) {
        Ok(pack) => pack,
        Err(_) => return Ok(false),
    };
    let present: HashSet<ObjectId> = pack.entries.iter().map(|object| object.entry.oid).collect();
    for object in &pack.entries {
        let body = &object.object.body;
        match object.object.object_type {
            ObjectType::Tree => {
                for entry in TreeEntries::new(format, body).flatten() {
                    if !entry.is_gitlink() && !present.contains(&entry.oid) {
                        return Ok(true);
                    }
                }
            }
            ObjectType::Commit => {
                if let Ok(commit) = Commit::parse_ref(format, body) {
                    if !present.contains(&commit.tree) {
                        return Ok(true);
                    }
                    for parent in &commit.parents {
                        if !present.contains(parent) {
                            return Ok(true);
                        }
                    }
                }
            }
            ObjectType::Tag => {
                if let Ok(tag) = Tag::parse_ref(format, body)
                    && !present.contains(&tag.object)
                {
                    return Ok(true);
                }
            }
            ObjectType::Blob => {}
        }
    }
    Ok(false)
}

/// fsck every object carried by `pack_bytes`, printing content findings in
/// git's `index-pack --strict` format (`warning: object <oid>: <id>: <detail>`
/// / `error: object <oid>: <id>: <detail>`). Returns the process exit code: 0
/// when no error-severity finding fired, 1 otherwise. Warnings never fail the
/// command, matching builtin/index-pack.c's fsck pass (which leaves the default
/// severities intact rather than applying the connectivity `--strict` promote).
pub(crate) fn fsck_pack_objects(
    pack_bytes: &[u8],
    format: ObjectFormat,
    overrides: &[String],
) -> Result<i32> {
    let pack = match sley_pack::PackFile::parse(pack_bytes, format) {
        Ok(pack) => pack,
        // A pack that does not even parse is reported by the index step; the
        // fsck pass simply has nothing to inspect.
        Err(_) => return Ok(0),
    };

    let mut severity = sley_fsck::content::SeverityConfig::new(false);
    for token in overrides {
        if let Some((id, value)) = token.split_once('=') {
            severity.set(id, value);
        }
    }

    let reader = PackObjectReader::new(format, &pack);
    let object_ids = pack
        .entries
        .iter()
        .map(|object| object.entry.oid)
        .collect::<Vec<_>>();
    let report = sley_fsck::fsck_objects_with_options(
        &reader,
        format,
        [],
        object_ids,
        sley_fsck::FsckOptions {
            severity: severity.clone(),
            ..Default::default()
        },
    );
    let mut had_error = false;
    for issue in &report.issues {
        match issue.stream {
            sley_fsck::IssueStream::Stderr => {
                if print_index_pack_fsck_issue(&issue.message) {
                    had_error = true;
                }
            }
            sley_fsck::IssueStream::Stdout => {
                eprintln!("error: {}", issue.message);
                if issue.severity == sley_fsck::IssueSeverity::Error {
                    had_error = true;
                }
            }
        }
    }
    if had_error || report.exit_code() != 0 {
        return Ok(1);
    }

    // Keep the simple per-object loop as a backstop for content-only findings
    // while the full fsck walker above carries the tree-context checks.
    let mut had_error = false;
    for object in &pack.entries {
        let findings = sley_fsck::content::check_object_content_with_format(
            format,
            object.object.object_type,
            &object.object.body,
            &severity,
        );
        for finding in findings {
            let label = match finding.severity {
                sley_fsck::content::Severity::Error => "error",
                sley_fsck::content::Severity::Warn => "warning",
                sley_fsck::content::Severity::Ignore => continue,
            };
            if let Some(raw) = &finding.raw_stderr {
                eprintln!("error: {raw}");
            }
            eprintln!(
                "{label}: object {}: {}: {}",
                object.entry.oid,
                finding.msg_id.camel(),
                finding.detail
            );
            if matches!(finding.severity, sley_fsck::content::Severity::Error) {
                had_error = true;
            }
        }
    }
    Ok(if had_error { 1 } else { 0 })
}

struct PackObjectReader {
    format: ObjectFormat,
    objects: HashMap<ObjectId, Arc<EncodedObject>>,
}

impl PackObjectReader {
    fn new(format: ObjectFormat, pack: &sley_pack::PackFile) -> Self {
        let objects = pack
            .entries
            .iter()
            .map(|entry| (entry.entry.oid, Arc::new(entry.object.clone())))
            .collect();
        Self { format, objects }
    }
}

impl ObjectReader for PackObjectReader {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        if *oid == ObjectId::empty_tree(self.format) {
            return Ok(Arc::new(EncodedObject::new(
                sley_object::ObjectType::Tree,
                Vec::new(),
            )));
        }
        self.objects
            .get(oid)
            .cloned()
            .ok_or_else(|| GitError::NotFound(sley_core::NotFoundKind::Message(oid.to_string())))
    }
}

fn print_index_pack_fsck_issue(message: &str) -> bool {
    if let Some(rest) = message.strip_prefix("error in ") {
        print_index_pack_fsck_content("error", rest);
        return true;
    }
    if let Some(rest) = message.strip_prefix("warning in ") {
        print_index_pack_fsck_content("warning", rest);
        return false;
    }
    eprintln!("{message}");
    message.starts_with("error:")
}

fn print_index_pack_fsck_content(label: &str, rest: &str) {
    if let Some((_, oid_and_detail)) = rest.split_once(' ')
        && let Some((oid, detail)) = oid_and_detail.split_once(": ")
    {
        eprintln!("{label}: object {oid}: {detail}");
        return;
    }
    eprintln!("{label}: object {rest}");
}

fn write_index_pack_output(path: &Path, index: &[u8]) -> Result<()> {
    // `index-pack -o <path>` is an explicit caller-chosen output. Upstream's
    // test suite reuses that path across different packs, so replace any prior
    // file instead of treating it like a content-addressed object component.
    fs::write(path, index).map_err(|err| GitError::Io(err.to_string()))
}

fn index_pack_usage<T>() -> Result<T> {
    eprintln!(
        "usage: git index-pack [-v] [-o <index-file>] [--keep | --keep=<msg>] [--[no-]rev-index] [--verify] [--strict[=<msg-id>=<severity>...]] [--fsck-objects[=<msg-id>=<severity>...]] (<pack-file> | --stdin [--fix-thin] [<pack-file>])"
    );
    Err(GitError::Exit(129))
}

#[derive(Debug)]
struct VerifyPackOptions {
    verbose: bool,
    stat_only: bool,
    format: Option<ObjectFormat>,
    index_paths: Vec<PathBuf>,
}

pub(crate) fn cmd_verify_pack(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let options = setup_verify_pack_options(args)?;
    // verify-pack inspects the named pack files directly, so it works outside a
    // repository. An explicit format wins; otherwise inherit a gently
    // discovered repository's format and finally fall back to SHA-1.
    let format = match options.format {
        Some(format) => format,
        None => match cli_session.open_repository() {
            Ok(repository) => repository.object_format(),
            Err(GitError::NotFound(_)) => ObjectFormat::Sha1,
            Err(err) => return Err(err),
        },
    };
    for index_path in &options.index_paths {
        verify_pack_one(format, index_path, options.verbose, options.stat_only)?;
    }
    Ok(())
}

fn setup_verify_pack_options(args: &[String]) -> Result<VerifyPackOptions> {
    let parsed = parse_options(args, verify_pack_option_specs(), VERIFY_PACK_USAGE)
        .map_err(cli_usage_error)?;
    let mut format = None;
    for option in &parsed.options {
        if option.long != Some("object-format") {
            continue;
        }
        if matches!(option.name, OptionName::NegatedLong("object-format")) {
            format = None;
        } else if let ParsedValue::Str(value) = &option.value {
            format = Some(parse_verify_pack_object_format(value)?);
        }
    }
    let index_paths = parsed
        .positionals
        .iter()
        .map(|path| PathBuf::from(*path))
        .collect::<Vec<_>>();
    if index_paths.is_empty() {
        return verify_pack_usage();
    }
    Ok(VerifyPackOptions {
        verbose: parsed.last_bool("verbose", false),
        stat_only: parsed.last_bool("stat-only", false),
        format,
        index_paths,
    })
}

const VERIFY_PACK_USAGE: &[&str] =
    &["git verify-pack [-v | --verbose] [-s | --stat-only] [--] <pack>.idx..."];

fn verify_pack_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[
        opt_bool(Some('v'), Some("verbose"), OptFlags::NONE, "verbose"),
        opt_bool(
            Some('s'),
            Some("stat-only"),
            OptFlags::NONE,
            "show statistics only",
        ),
        opt_str(
            None,
            Some("object-format"),
            "<hash>",
            OptFlags::NONE,
            "specify the hash algorithm to use",
        ),
    ];
    SPECS
}

fn verify_pack_one(
    format: ObjectFormat,
    index_path: &Path,
    verbose: bool,
    stat_only: bool,
) -> Result<()> {
    // Upstream verify-pack accepts "foo.pack", "foo.idx" and "foo", and
    // normalizes them all to the pack/idx pair (builtin/verify-pack.c's
    // verify_one_pack). Derive the .idx path the same way.
    let base_path = {
        let path = index_path.to_string_lossy();
        let base = path
            .strip_suffix(".idx")
            .or_else(|| path.strip_suffix(".pack"))
            .unwrap_or(&path);
        base.to_string()
    };
    let index_path = PathBuf::from(format!("{base_path}.idx"));
    let pack_path = PathBuf::from(format!("{base_path}.pack"));

    let index = PackIndex::parse(&fs::read(&index_path)?, format)?;

    // verify-pack validates the *named pack file*, not the object database:
    // parse the pack (checking its trailing checksum + every object's inflate)
    // and cross-check it against the `.idx`. A mismatched `.idx`/`.pack` pair, a
    // corrupted signature/version, or a damaged object all fail here, like
    // builtin/verify-pack.c -> verify_pack -> verify_packfile.
    let pack_bytes = match fs::read(&pack_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("fatal: cannot open packfile {}: {err}", pack_path.display());
            return Err(GitError::Exit(1));
        }
    };
    // `verify_pack_stats` parses + resolves the pack (validating checksum,
    // inflate, and delta chains) and returns the per-object report git prints.
    let stats = match sley_pack::PackFile::verify_pack_stats(&pack_bytes, format) {
        Ok(stats) => stats,
        Err(err) => {
            eprintln!("error: {err}");
            eprintln!("fatal: packfile {} cannot be verified", pack_path.display());
            return Err(GitError::Exit(1));
        }
    };

    // The pack's trailing checksum must equal the one the `.idx` records.
    if stats.checksum != index.pack_checksum {
        eprintln!(
            "fatal: {}: pack checksum mismatch with index",
            pack_path.display()
        );
        return Err(GitError::Exit(1));
    }

    // Every object the index advertises must exist in the pack at the same
    // offset with the same id, and the pack must hold exactly that object set.
    let mut stat_by_offset: HashMap<u64, &sley_pack::PackVerifyStat> =
        HashMap::with_capacity(stats.objects.len());
    for object in &stats.objects {
        stat_by_offset.insert(object.offset, object);
    }
    if stats.objects.len() != index.entries.len() {
        eprintln!(
            "fatal: {}: object count mismatch between pack and index",
            pack_path.display()
        );
        return Err(GitError::Exit(1));
    }
    for entry in &index.entries {
        match stat_by_offset.get(&entry.offset) {
            Some(object) if object.oid == entry.oid => {}
            _ => {
                eprintln!(
                    "fatal: {}: object {} at offset {} does not match the pack",
                    pack_path.display(),
                    entry.oid,
                    entry.offset
                );
                return Err(GitError::Exit(1));
            }
        }
    }

    // Reproduce builtin/index-pack.c::show_pack_info exactly. Objects print in
    // pack offset order (verify_pack_stats already sorts them that way). The
    // per-object line is `<oid> <type-6> <size> <size-in-pack> <offset>` with an
    // optional ` <depth> <base-oid>` suffix for delta entries. The histogram is
    // `non delta: N objects` followed by `chain length = K: M objects` per depth.
    let non_delta = stats
        .objects
        .iter()
        .filter(|object| object.delta_depth == 0)
        .count();
    let deepest = stats
        .objects
        .iter()
        .map(|object| object.delta_depth)
        .max()
        .unwrap_or(0);
    let mut chain_histogram = vec![0usize; deepest as usize];
    for object in &stats.objects {
        if object.delta_depth > 0 {
            chain_histogram[(object.delta_depth - 1) as usize] += 1;
        }
        if verbose && !stat_only {
            print!(
                "{} {:<6} {} {} {}",
                object.oid,
                object.object_type.as_str(),
                object.size,
                object.size_in_pack,
                object.offset
            );
            if let Some(base_oid) = &object.base_oid {
                print!(" {} {base_oid}", object.delta_depth);
            }
            println!();
        }
    }
    if verbose || stat_only {
        // git only emits the "non delta" line when there is at least one, and
        // uses the singular noun for a count of one (printf_ln + Q_()).
        if non_delta > 0 {
            println!("non delta: {non_delta} {}", plural_objects(non_delta));
        }
        for (depth, &count) in chain_histogram.iter().enumerate() {
            if count == 0 {
                continue;
            }
            println!(
                "chain length = {}: {count} {}",
                depth + 1,
                plural_objects(count)
            );
        }
        if verbose && !stat_only {
            println!("{}: ok", pack_path.display());
        }
    }
    Ok(())
}

/// git's `Q_("... object", "... objects", n)` noun selection: singular for 1.
fn plural_objects(count: usize) -> &'static str {
    if count == 1 { "object" } else { "objects" }
}

fn parse_verify_pack_object_format(value: &str) -> Result<ObjectFormat> {
    match value {
        "sha1" => Ok(ObjectFormat::Sha1),
        "sha256" => Ok(ObjectFormat::Sha256),
        _ => {
            eprintln!("fatal: unknown hash algorithm '{value}'");
            Err(GitError::Exit(1))
        }
    }
}

fn verify_pack_usage<T>() -> Result<T> {
    eprintln!("usage: git verify-pack [-v | --verbose] [-s | --stat-only] [--] <pack>.idx...");
    eprintln!();
    eprintln!("    -v, --[no-]verbose    verbose");
    eprintln!("    -s, --[no-]stat-only  show statistics only");
    eprintln!("    --[no-]object-format <hash>");
    eprintln!("                          specify the hash algorithm to use");
    eprintln!();
    Err(GitError::Exit(129))
}

fn expand_repack_short_clusters(args: &[String]) -> Vec<String> {
    let mut expanded = Vec::with_capacity(args.len());
    for arg in args {
        let bytes = arg.as_bytes();
        if bytes.len() > 2
            && bytes[0] == b'-'
            && bytes[1..].iter().all(|&ch| {
                matches!(
                    ch,
                    b'a' | b'A' | b'b' | b'd' | b'f' | b'F' | b'k' | b'l' | b'm' | b'n' | b'q'
                )
            })
        {
            expanded.extend(bytes[1..].iter().map(|&ch| format!("-{}", ch as char)));
        } else {
            expanded.push(arg.clone());
        }
    }
    expanded
}

pub(crate) fn cmd_repack(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut options = gc_repack::RepackCommandOptions::default();
    let mut iter = expand_repack_short_clusters(args).into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-d" => options.prune = true,
            "-q" | "--quiet" => options.quiet = true,
            "-b" | "--write-bitmap-index" => options.write_bitmaps = Some(true),
            "--no-write-bitmap-index" => options.write_bitmaps = Some(false),
            "-a" => options.all = true,
            "-A" => {
                options.all = true;
                options.unpack_unreachable = true;
            }
            "-m" | "--write-midx" => options.write_midx = true,
            "-l" | "--local" => options.local = true,
            "-n" => options.update_server_info = Some(false),
            "--cruft" => options.cruft = true,
            "--path-walk" => {}
            "--no-path-walk" => {}
            "--cruft-expiration" => {
                options.cruft = true;
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `cruft-expiration' requires a value".into())
                })?;
                options.cruft_expiration = Some(gc_repack::parse_cruft_expiration(&value)?);
            }
            value if value.starts_with("--cruft-expiration=") => {
                options.cruft = true;
                options.cruft_expiration = Some(gc_repack::parse_cruft_expiration(
                    &value["--cruft-expiration=".len()..],
                )?);
            }
            "--expire-to" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `expire-to' requires a value".into())
                })?;
                options.expire_to = Some(value);
            }
            value if value.starts_with("--expire-to=") => {
                options.expire_to = Some(value["--expire-to=".len()..].to_string());
            }
            "--max-pack-size" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `max-pack-size' requires a value".into())
                })?;
                options.max_pack_size = Some(gc_engine::parse_gc_size(&value)?);
            }
            value if value.starts_with("--max-pack-size=") => {
                options.max_pack_size =
                    Some(gc_engine::parse_gc_size(&value["--max-pack-size=".len()..])?);
            }
            "--max-cruft-size" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `max-cruft-size' requires a value".into())
                })?;
                options.max_cruft_size = Some(gc_engine::parse_gc_size(&value)?);
            }
            value if value.starts_with("--max-cruft-size=") => {
                options.max_cruft_size =
                    Some(gc_engine::parse_gc_size(&value["--max-cruft-size=".len()..])?);
            }
            "--combine-cruft-below-size" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `combine-cruft-below-size' requires a value".into())
                })?;
                options.combine_cruft_below_size = Some(gc_engine::parse_gc_size(&value)?);
            }
            value if value.starts_with("--combine-cruft-below-size=") => {
                options.combine_cruft_below_size = Some(gc_engine::parse_gc_size(
                    &value["--combine-cruft-below-size=".len()..],
                )?);
            }
            "--window" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("option `window' requires a value".into()))?;
                options.window = Some(gc_repack::parse_repack_window(&value)?);
            }
            value if value.starts_with("--window=") => {
                options.window = Some(gc_repack::parse_repack_window(&value["--window=".len()..])?);
            }
            "--filter" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("option `filter' requires a value".into()))?;
                options.filter_specs.push(value);
            }
            value if value.starts_with("--filter=") => {
                options.filter_specs.push(value["--filter=".len()..].to_string());
            }
            "--filter-to" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `filter-to' requires a value".into())
                })?;
                options.filter_to = Some(value);
            }
            value if value.starts_with("--filter-to=") => {
                options.filter_to = Some(value["--filter-to=".len()..].to_string());
            }
            value if value.starts_with("--name-hash-version=") => {
                let raw = &value["--name-hash-version=".len()..];
                options.name_hash_version = Some(raw.parse::<i32>().map_err(|_| {
                    GitError::Command(format!("invalid --name-hash-version option: {raw}"))
                })?);
            }
            "--name-hash-version" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `name-hash-version' requires a value".into())
                })?;
                options.name_hash_version = Some(value.parse::<i32>().map_err(|_| {
                    GitError::Command(format!("invalid --name-hash-version option: {value}"))
                })?);
            }
            "--unpack-unreachable" => {
                options.unpack_unreachable = true;
                options.unpack_unreachable_before = Some(None);
            }
            value if value.starts_with("--unpack-unreachable=") => {
                options.unpack_unreachable = true;
                options.unpack_unreachable_before = Some(gc_repack::parse_cruft_expiration(
                    &value["--unpack-unreachable=".len()..],
                )?);
            }
            "-k" | "--keep-unreachable" => options.keep_unreachable = true,
            "--pack-kept-objects" => options.pack_kept_objects = true,
            "-g" | "--geometric" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `geometric' requires a value".into())
                })?;
                options.geometric = Some(gc_repack::parse_geometric_factor(&value)?);
            }
            "--keep-pack" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `keep-pack' requires a value".into())
                })?;
                options.keep_packs.push(gc_repack::strip_pack_suffix(&value));
            }
            value if value.starts_with("--geometric=") => {
                options.geometric =
                    Some(gc_repack::parse_geometric_factor(&value["--geometric=".len()..])?);
            }
            value if value.starts_with("--keep-pack=") => {
                options.keep_packs
                    .push(gc_repack::strip_pack_suffix(&value["--keep-pack=".len()..]));
            }
            "--no-cruft" => options.cruft = false,
            "-f" | "-F" => options.force_rewrite = true,
            // Accepted no-ops.
            "--progress" | "--no-progress" | "--no-pack-kept-objects" => {}
            value
                if value.starts_with("--depth")
                    || value.starts_with("--threads")
                    || value.starts_with("--pack.packSizeLimit") => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported repack option {value}"
                )));
            }
            value => {
                return Err(GitError::Command(format!(
                    "unsupported repack argument {value}"
                )));
            }
        }
    }
    run_repack(cli_session, options)
}


// ---------------------------------------------------------------------------
// gc/repack/maintenance engine wrappers (sley-gc). The CLI keeps argv parsing,
// usage/help rendering, stdout formatting, hooks, and exit codes; everything
// else lives in `sley_gc`.
// ---------------------------------------------------------------------------

use sley_gc::maintenance::{
    self as gc_maintenance, validate_maintenance_schedule, MaintenanceScheduler,
};
use sley_gc::midx as gc_midx;
use sley_gc::count_objects as gc_count_objects;
use sley_gc::gc::{self as gc_engine, GcAutoMode, GcOptions};
use sley_gc::prune as gc_prune;
use sley_gc::repack as gc_repack;
use sley_gc::trace2 as gc_trace2;
use sley_gc::GcServices;

pub(crate) fn trace2_child_start(args: &[&str]) {
    gc_trace2::child_start(args);
}

pub(crate) fn trace2_touch() {
    gc_trace2::touch();
}

pub(crate) fn reftable_lock_timeout_override() -> Result<Option<u64>> {
    Ok(global_config_value("reftable.lockTimeout")?.and_then(|value| value.parse::<u64>().ok()))
}

pub(crate) fn run_repack(
    cli_session: &crate::session::CliSession,
    options: gc_repack::RepackCommandOptions,
) -> Result<()> {
    let git_dir = cli_session.git_dir()?;
    let mut services = GcServices {
        git_trace_line: &crate::setup::git_trace_line,
        replace_objects: cli_session.replace_objects(),
        pack_refs_all_prune: &mut || Ok(()),
        reflog_expire: &mut |_| Ok(()),
        commit_graph_write_reachable: &mut |_| Ok(()),
        update_server_info: &mut || {
            let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
            crate::commands::refs::update_server_info_at(&common_git_dir, &[])
        },
        pre_auto_gc_hook_ok: None,
        reftable_lock_timeout: reftable_lock_timeout_override()?,
        has_promisor_remote: Some(&sley_remote::config_has_promisor_remote),
        hydrate_promisor_remotes: Some(&mut |dir: &Path, format, roots| {
            sley_remote::hydrate_reachable_from_local_promisor_remotes(dir, format, roots)
        }),
    };
    gc_repack::run_repack(&mut services, &git_dir, cli_session.replace_objects(), &options)
}

pub(crate) fn cmd_count_objects(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut verbose = false;
    let mut human_readable = false;
    for arg in args {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "-H" | "--human-readable" => human_readable = true,
            "--no-human-readable" => human_readable = false,
            value
                if value.starts_with('-')
                    && !value.starts_with("--")
                    && value.len() > 2
                    && value[1..].chars().all(|flag| matches!(flag, 'v' | 'H')) =>
            {
                for flag in value[1..].chars() {
                    match flag {
                        'v' => verbose = true,
                        'H' => human_readable = true,
                        _ => {}
                    }
                }
            }
            value => {
                return Err(GitError::Command(format!(
                    "count-objects currently supports -v/--verbose and -H/--human-readable with negations; unsupported option {value}"
                )));
            }
        }
    }
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let stats = gc_count_objects::count_objects_stats(&git_dir, format)?;
    if verbose {
        println!("count: {}", stats.count);
        println!(
            "size: {}",
            gc_count_objects::count_objects_size(stats.size_kib, human_readable)
        );
        println!("in-pack: {}", stats.in_pack);
        println!("packs: {}", stats.packs);
        println!(
            "size-pack: {}",
            gc_count_objects::count_objects_pack_size(stats.size_pack_bytes, human_readable)
        );
        println!("prune-packable: {}", stats.prune_packable);
        println!("garbage: {}", stats.garbage);
        println!(
            "size-garbage: {}",
            gc_count_objects::count_objects_pack_size(stats.size_garbage_bytes, human_readable)
        );
        for alternate in &stats.alternates {
            println!("alternate: {alternate}");
        }
    } else {
        println!(
            "{} objects, {}",
            stats.count,
            if human_readable {
                gc_count_objects::count_objects_human_size(stats.size_kib)
            } else {
                format!("{} kilobytes", stats.size_kib)
            }
        );
    }
    Ok(())
}

pub(crate) fn verify_midx_at(
    object_dir: &Path,
    format: ObjectFormat,
    progress: bool,
) -> Result<()> {
    gc_midx::verify_midx_at(object_dir, format, progress)
}





pub(crate) fn cmd_gc(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let options = setup_gc_options(args)?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let config = read_repo_config(&common_git_dir)?;
    gc_engine::validate_gc_prune_expire(&config, &common_git_dir)?;

    // gc.cruftPacks defaults to true (cruft packs are git's default since 2.42).
    let cruft_packs = options
        .cruft_flag
        .or_else(|| config.get_bool("gc", None, "cruftPacks"))
        .unwrap_or(true);

    // gc.pruneExpire defaults to "2.weeks.ago"; --prune=<date>/--no-prune and the
    // config override it. None means "never prune" (`--no-prune`).
    let prune_expire: Option<String> = match options.prune_override.clone() {
        Some(value) => value,
        None => Some(
            config
                .get("gc", None, "pruneExpire")
                .unwrap_or("2.weeks.ago")
                .to_string(),
        ),
    };

    let auto_mode = if options.auto {
        match gc_engine::gc_auto_mode(&common_git_dir, format, &config)? {
            Some(mode) => mode,
            None => return Ok(()),
        }
    } else {
        GcAutoMode::Full
    };
    if options.auto {
        if gc_engine::gc_recent_log_blocks_auto(&common_git_dir, &config)? {
            return Ok(());
        }
        if gc_engine::gc_lock_held(&common_git_dir)? && !options.force {
            return Ok(());
        }
        if commands::hooks::run_hook(
            cli_session,
            "pre-auto-gc",
            commands::hooks::HookRun::default(),
        )
        .is_err()
        {
            return Ok(());
        }
        if !options.quiet {
            if gc_engine::gc_should_detach(&config, options.detach) {
                eprintln!("Auto packing the repository in background for optimum performance.");
            } else {
                eprintln!("Auto packing the repository for optimum performance.");
            }
            eprintln!("See \"git help gc\" for manual housekeeping.");
        }
    } else if gc_engine::gc_lock_held(&common_git_dir)? && !options.force {
        eprintln!("fatal: gc is already running");
        return Err(GitError::Exit(128));
    }

    gc_engine::gc_write_pid(&common_git_dir)?;
    let common_for_services = common_git_dir.clone();
    let git_dir_for_reflog = git_dir.clone();
    let mut services = GcServices {
        git_trace_line: &crate::setup::git_trace_line,
        replace_objects: cli_session.replace_objects(),
        pack_refs_all_prune: &mut || {
            crate::commands::pack::cmd_pack_refs(
                cli_session,
                &["--all".to_string(), "--prune".to_string()],
            )
        },
        reflog_expire: &mut |expire_args: &[String]| {
            let _ = crate::commands::refs::reflog_expire_at(
                &git_dir_for_reflog,
                expire_args,
                cli_session.replace_objects(),
            );
            Ok(())
        },
        commit_graph_write_reachable: &mut |progress: bool| {
            let progress_arg = if progress { "--progress" } else { "--no-progress" };
            commands::plumbing::cmd_commit_graph(
                cli_session,
                &[
                    "write".to_string(),
                    "--reachable".to_string(),
                    progress_arg.to_string(),
                ],
            )
        },
        update_server_info: &mut || {
            crate::commands::refs::update_server_info_at(&common_for_services, &[])
        },
        pre_auto_gc_hook_ok: None,
        reftable_lock_timeout: reftable_lock_timeout_override()?,
        has_promisor_remote: None,
        hydrate_promisor_remotes: None,
    };
    let result = gc_engine::gc_run_locked(
        &mut services,
        &git_dir,
        &common_git_dir,
        format,
        &config,
        &options,
        cruft_packs,
        prune_expire,
        auto_mode,
    );
    let _ = fs::remove_file(common_git_dir.join("gc.pid"));
    if result.is_ok() && !options.auto {
        let _ = fs::remove_file(common_git_dir.join("gc.log"));
    }
    result
}

fn setup_gc_options(args: &[String]) -> Result<GcOptions> {
    let parsed = parse_options(args, gc_option_specs(), GC_USAGE).map_err(cli_usage_error)?;
    if !parsed.positionals.is_empty() {
        return gc_usage();
    }
    let mut options = gc_engine::GcOptions::default();
    options.quiet = parsed.last_bool("quiet", false);
    options.auto = parsed.last_bool("auto", false);
    options.force = parsed.last_bool("force", false);
    options.skip_foreground_tasks = parsed
        .options
        .iter()
        .any(|option| option.long == Some("skip-foreground-tasks"));
    options.aggressive = parsed.last_bool("aggressive", false);
    options.detach = last_tri_state_bool(&parsed, "detach");
    options.keep_largest_pack = last_tri_state_bool(&parsed, "keep-largest-pack");
    options.cruft_flag = last_tri_state_bool(&parsed, "cruft");
    for option in &parsed.options {
        match option.long {
            Some("prune") => match &option.name {
                OptionName::NegatedLong(_) => options.prune_override = Some(None),
                OptionName::Long(_) => match &option.value {
                    ParsedValue::Str(value) if !value.is_empty() => {
                        options.prune_override = Some(Some(value.to_string()));
                    }
                    _ => options.prune_override = Some(Some("2.weeks.ago".to_string())),
                },
                _ => {}
            },
            Some("max-cruft-size") => {
                if let ParsedValue::Str(value) = &option.value {
                    options.max_cruft_size = Some(gc_engine::parse_gc_size(value)?);
                }
            }
            Some("expire-to") => {
                if let ParsedValue::Str(value) = &option.value {
                    options.expire_to = Some(value.to_string());
                }
            }
            _ => {}
        }
    }
    Ok(options)
}

const GC_USAGE: &[&str] = &["git gc [<options>]"];

fn gc_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[
        opt_bool(
            Some('q'),
            Some("quiet"),
            OptFlags::NONE,
            "suppress progress reporting",
        ),
        opt_str(
            None,
            Some("prune"),
            "<date>",
            OptFlags::OPTARG.union(OptFlags::NONE),
            "prune unreferenced objects",
        ),
        opt_bool(
            None,
            Some("cruft"),
            OptFlags::NONE,
            "pack unreferenced objects separately",
        ),
        opt_bool(
            None,
            Some("aggressive"),
            OptFlags::NONE,
            "be more thorough (increased runtime)",
        ),
        opt_bool(None, Some("auto"), OptFlags::NONE, "enable auto-gc mode"),
        opt_bool(
            None,
            Some("detach"),
            OptFlags::NONE,
            "perform garbage collection in the background",
        ),
        opt_bool(
            None,
            Some("force"),
            OptFlags::NONE,
            "force running gc even if there may be another gc running",
        ),
        opt_bool(
            None,
            Some("keep-largest-pack"),
            OptFlags::NONE,
            "repack all other packs except the largest pack",
        ),
        opt_str(
            None,
            Some("max-cruft-size"),
            "<size>",
            OptFlags::NONE,
            "maximum cruft pack size",
        ),
        opt_str(
            None,
            Some("expire-to"),
            "<dir>",
            OptFlags::NONE,
            "pack prefix to store a cruft pack",
        ),
        opt_bool(None, Some("skip-foreground-tasks"), OptFlags::NONEG, ""),
        opt_bool(None, Some("progress"), OptFlags::HIDDEN, ""),
    ];
    SPECS
}

fn gc_usage<T>() -> Result<T> {
    eprintln!("usage: git gc [<options>]");
    eprintln!();
    eprintln!("    -q, --[no-]quiet      suppress progress reporting");
    eprintln!("    --[no-]prune[=<date>] prune unreferenced objects");
    eprintln!("    --[no-]cruft          pack unreferenced objects separately");
    eprintln!("    --[no-]aggressive     be more thorough (increased runtime)");
    eprintln!("    --[no-]auto           enable auto-gc mode");
    eprintln!("    --[no-]detach         perform garbage collection in the background");
    eprintln!("    --[no-]force          force running gc even if there may be another gc running");
    eprintln!("    --[no-]keep-largest-pack");
    eprintln!("                          repack all other packs except the largest pack");
    Err(GitError::Exit(129))
}


pub(crate) fn cmd_maintenance(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        eprintln!("error: need a subcommand");
        return maintenance_usage();
    };
    match subcommand {
        // `-h` before any subcommand prints the top-level usage (rc 129). Unlike
        // the error paths, an explicit help request goes to STDOUT with a
        // trailing blank line, matching parse-options' usage_with_options.
        "-h" | "--help" => {
            println!("usage: git maintenance <subcommand> [<options>]");
            println!();
            Err(GitError::Exit(129))
        }
        "run" => cmd_maintenance_run(cli_session, &args[1..]),
        "is-needed" => cmd_maintenance_is_needed(cli_session, &args[1..]),
        "register" => cmd_maintenance_register(cli_session, &args[1..]),
        "unregister" => cmd_maintenance_unregister(cli_session, &args[1..]),
        "start" => cmd_maintenance_start(cli_session, &args[1..]),
        "stop" => cmd_maintenance_stop(cli_session, &args[1..]),
        // git's parse-options subcommand dispatch quotes the offending token with
        // a backtick + apostrophe, not a matched pair (parse-options.c).
        _ => {
            eprintln!("error: unknown subcommand: `{subcommand}'");
            maintenance_usage()
        }
    }
}

fn maintenance_usage<T>() -> Result<T> {
    eprintln!("usage: git maintenance <subcommand> [<options>]");
    Err(GitError::Exit(129))
}

fn maintenance_run_usage<T>() -> Result<T> {
    eprintln!("usage: git maintenance run [--auto] [--[no-]quiet] [--task=<task>] [--schedule]");
    eprintln!();
    eprintln!("    --[no-]auto           run tasks based on the state of the repository");
    eprintln!("    --[no-]detach         perform maintenance in the background");
    eprintln!("    --[no-]schedule <frequency>");
    eprintln!("                          run tasks based on frequency");
    eprintln!("    --[no-]quiet          do not report progress or other information over stderr");
    eprintln!("    --task <task>         run a specific task");
    Err(GitError::Exit(129))
}


fn cmd_maintenance_run(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut quiet = true;
    let mut auto = false;
    let mut detach = false;
    let mut schedule: Option<String> = None;
    // `--task=` selections in command-line order, validated as we parse so the
    // "not a valid task" / "cannot be selected multiple times" diagnostics fire
    // in git's order (task_option_parse, builtin/gc.c).
    let mut tasks: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--auto" => auto = true,
            "--no-auto" => auto = false,
            "--detach" => detach = true,
            "--no-detach" => detach = false,
            "--schedule" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("error: option `schedule' requires a value");
                    return Err(GitError::Exit(129));
                };
                schedule = Some(validate_maintenance_schedule(value)?);
            }
            value if let Some(freq) = value.strip_prefix("--schedule=") => {
                schedule = Some(validate_maintenance_schedule(freq)?);
            }
            "--task" => {
                index += 1;
                let Some(task) = args.get(index) else {
                    eprintln!("error: option `task' requires a value");
                    return Err(GitError::Exit(129));
                };
                push_maintenance_task(&mut tasks, task)?;
            }
            value if let Some(task) = value.strip_prefix("--task=") => {
                push_maintenance_task(&mut tasks, task)?;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                return maintenance_run_usage();
            }
            _ => return maintenance_run_usage(),
        }
        index += 1;
    }

    // `--auto`/`--task=` are each incompatible with `--schedule=` (git's
    // die_for_incompatible_opt2 pair, builtin/gc.c maintenance_run).
    if auto && schedule.is_some() {
        eprintln!("fatal: options '--auto' and '--schedule=' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if !tasks.is_empty() && schedule.is_some() {
        eprintln!("fatal: options '--task=' and '--schedule=' cannot be used together");
        return Err(GitError::Exit(128));
    }

    trace2_touch();
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let config = read_repo_config(&common_git_dir)?;
    let selected =
        gc_maintenance::maintenance_select_tasks(&config, &tasks, schedule.as_deref())?;
    let mut services = GcServices {
        git_trace_line: &crate::setup::git_trace_line,
        replace_objects: cli_session.replace_objects(),
        pack_refs_all_prune: &mut || Ok(()),
        reflog_expire: &mut |_| Ok(()),
        commit_graph_write_reachable: &mut |progress: bool| {
            let progress_arg = if progress { "--progress" } else { "--no-progress" };
            commands::plumbing::cmd_commit_graph(
                cli_session,
                &[
                    "write".to_string(),
                    "--reachable".to_string(),
                    progress_arg.to_string(),
                ],
            )
        },
        update_server_info: &mut || Ok(()),
        pre_auto_gc_hook_ok: None,
        reftable_lock_timeout: reftable_lock_timeout_override()?,
        has_promisor_remote: None,
        hydrate_promisor_remotes: None,
    };
    gc_maintenance::maintenance_run_selected(
        &mut services,
        &common_git_dir,
        &config,
        &selected,
        quiet,
        auto,
        detach,
    )?;
    Ok(())
}



/// Append a `--task=<name>` selection, mirroring git's `task_option_parse`:
/// reject an unknown task name, and reject a task already selected (both rc 129).
fn push_maintenance_task(tasks: &mut Vec<String>, task: &str) -> Result<()> {
    let task = if task.eq_ignore_ascii_case("refs optimize") {
        "pack-refs"
    } else {
        task
    };
    if !gc_maintenance::MAINTENANCE_TASKS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(task))
    {
        eprintln!("error: '{task}' is not a valid task");
        return Err(GitError::Exit(129));
    }
    if tasks.iter().any(|seen| seen.eq_ignore_ascii_case(task)) {
        eprintln!("error: task '{task}' cannot be selected multiple times");
        return Err(GitError::Exit(129));
    }
    tasks.push(task.to_string());
    Ok(())
}



fn cmd_maintenance_is_needed(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut auto = false;
    let mut schedule = None;
    let mut tasks = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--auto" => auto = true,
            "--no-auto" => auto = false,
            "--schedule" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return maintenance_run_usage();
                };
                schedule = Some(validate_maintenance_schedule(value)?);
            }
            value if let Some(value) = value.strip_prefix("--schedule=") => {
                schedule = Some(validate_maintenance_schedule(value)?);
            }
            "--task" => {
                index += 1;
                let Some(task) = args.get(index) else {
                    return maintenance_run_usage();
                };
                push_maintenance_task(&mut tasks, task)?;
            }
            value if let Some(task) = value.strip_prefix("--task=") => {
                push_maintenance_task(&mut tasks, task)?;
            }
            value if value.starts_with('-') => return maintenance_run_usage(),
            _ => return maintenance_run_usage(),
        }
        index += 1;
    }
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let config = read_repo_config(&common_git_dir)?;
    let selected =
        gc_maintenance::maintenance_select_tasks(&config, &tasks, schedule.as_deref())?;
    if (!auto && !selected.is_empty())
        || selected
            .iter()
            .any(|task| {
                gc_maintenance::maintenance_task_needed(&common_git_dir, &config, task)
                    .unwrap_or(false)
            })
    {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}




fn cmd_maintenance_register(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let config_file = parse_maintenance_config_file(args, "register")?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let repo = env::current_dir()?.display().to_string();

    let _ = gc_maintenance::report_missing_maintenance_repo(&common_git_dir);
    commands::config_cmd::cmd_config(
        cli_session,
        &[
            "set".to_string(),
            "maintenance.auto".to_string(),
            "false".to_string(),
        ],
    )?;
    if read_repo_config(&common_git_dir)?
        .get("maintenance", None, "strategy")
        .is_none()
    {
        commands::config_cmd::cmd_config(
            cli_session,
            &[
                "set".to_string(),
                "maintenance.strategy".to_string(),
                "incremental".to_string(),
            ],
        )?;
    }

    let file = config_file.unwrap_or(gc_maintenance::maintenance_global_config_path()?);
    gc_maintenance::config_add_value_if_missing(&file, "maintenance", "repo", &repo)?;
    Ok(())
}

fn cmd_maintenance_unregister(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let (config_file, force) = parse_maintenance_unregister_args(args)?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let repo = env::current_dir()?.display().to_string();
    let missing_repo_value =
        gc_maintenance::report_missing_maintenance_repo(&common_git_dir);
    if missing_repo_value && !force {
        return Err(GitError::Exit(128));
    }
    let file = config_file.unwrap_or(gc_maintenance::maintenance_global_config_path()?);
    if !gc_maintenance::config_remove_value(&file, "maintenance", "repo", &repo)? && !force {
        eprintln!("fatal: repository '{repo}' is not registered");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

fn parse_maintenance_config_file(args: &[String], subcommand: &str) -> Result<Option<PathBuf>> {
    let mut config_file = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config-file" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    return maintenance_subcommand_usage(subcommand);
                };
                config_file = Some(PathBuf::from(path));
            }
            value if let Some(path) = value.strip_prefix("--config-file=") => {
                config_file = Some(PathBuf::from(path));
            }
            _ => return maintenance_subcommand_usage(subcommand),
        }
        index += 1;
    }
    Ok(config_file)
}

fn parse_maintenance_unregister_args(args: &[String]) -> Result<(Option<PathBuf>, bool)> {
    let mut config_file = None;
    let mut force = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--force" | "-f" => force = true,
            "--config-file" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    return maintenance_subcommand_usage("unregister");
                };
                config_file = Some(PathBuf::from(path));
            }
            value if let Some(path) = value.strip_prefix("--config-file=") => {
                config_file = Some(PathBuf::from(path));
            }
            _ => return maintenance_subcommand_usage("unregister"),
        }
        index += 1;
    }
    Ok((config_file, force))
}

fn maintenance_subcommand_usage<T>(subcommand: &str) -> Result<T> {
    match subcommand {
        "register" => eprintln!("usage: git maintenance register [--config-file <path>]"),
        "unregister" => {
            eprintln!("usage: git maintenance unregister [--config-file <path>] [--force]")
        }
        "start" => eprintln!("usage: git maintenance start [--scheduler=<scheduler>]"),
        "stop" => eprintln!("usage: git maintenance stop"),
        _ => eprintln!("usage: git maintenance <subcommand> [<options>]"),
    }
    Err(GitError::Exit(129))
}




pub(crate) fn cmd_maintenance_start(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let scheduler = parse_maintenance_start_args(args)?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let scheduler = gc_maintenance::resolve_maintenance_scheduler(scheduler)?;
    gc_maintenance::validate_scheduler_available(scheduler)?;
    // Git installs the schedule before registering the repository.  In
    // particular, an unavailable scheduler must not leave maintenance.repo
    // behind even though Scalar treats that failure as a warning.
    gc_maintenance::update_background_schedule(&common_git_dir, Some(scheduler))?;
    cmd_maintenance_register(cli_session, &[])
}

pub(crate) fn cmd_maintenance_stop(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    if !args.is_empty() {
        return maintenance_subcommand_usage("stop");
    }
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    gc_maintenance::update_background_schedule(&common_git_dir, None)
}

fn parse_maintenance_start_args(args: &[String]) -> Result<Option<MaintenanceScheduler>> {
    let mut scheduler = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--scheduler" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return maintenance_subcommand_usage("start");
                };
                scheduler = parse_scheduler_option(value)?;
            }
            value if let Some(name) = value.strip_prefix("--scheduler=") => {
                scheduler = parse_scheduler_option(name)?;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                return maintenance_subcommand_usage("start");
            }
            _ => return maintenance_subcommand_usage("start"),
        }
        index += 1;
    }
    Ok(scheduler)
}

fn parse_scheduler_option(value: &str) -> Result<Option<MaintenanceScheduler>> {
    if value.eq_ignore_ascii_case("auto") {
        Ok(None)
    } else {
        parse_scheduler(value).map(Some)
    }
}

fn parse_scheduler(value: &str) -> Result<MaintenanceScheduler> {
    match value.to_ascii_lowercase().as_str() {
        "cron" | "crontab" => Ok(MaintenanceScheduler::Cron),
        "systemd" | "systemd-timer" => Ok(MaintenanceScheduler::Systemd),
        "launchctl" => Ok(MaintenanceScheduler::Launchctl),
        "schtasks" => Ok(MaintenanceScheduler::Schtasks),
        _ => {
            eprintln!("error: unrecognized --scheduler argument '{value}'");
            Err(GitError::Exit(129))
        }
    }
}



/// `git unpack-objects` — explode a pack stream from stdin into loose objects
/// (upstream `builtin/unpack-objects.c`). `-n` parses without writing; the
/// other upstream flags are accepted and inert for this in-process path.
pub(crate) fn cmd_unpack_objects(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut dry_run = false;
    let mut strict = false;
    for arg in args {
        match arg.as_str() {
            "-n" => dry_run = true,
            "--strict" => strict = true,
            "-q" | "-r" => {}
            value
                if value.starts_with("--pack_header=")
                    || value.starts_with("--max-input-size=") => {}
            value => {
                return Err(GitError::Command(format!(
                    "unpack-objects: unsupported option {value}"
                )));
            }
        }
    }
    let repository = cli_session.open_repository()?;
    let _git_dir = repository.git_dir();
    let format = repository.object_format();
    let mut pack_bytes = Vec::new();
    io::Read::read_to_end(&mut io::stdin().lock(), &mut pack_bytes)?;
    if dry_run {
        sley_pack::PackFile::parse(&pack_bytes, format)?;
        return Ok(());
    }
    if strict {
        let exit = fsck_pack_objects(&pack_bytes, format, &[])?;
        if exit != 0 {
            return Err(GitError::Exit(exit));
        }
    }
    sley_odb::unpack_packfile_objects(&pack_bytes, format, repository.object_database().loose())?;
    Ok(())
}


#[derive(Debug)]
struct PackRefsOptions {
    all: bool,
    prune: bool,
    auto: bool,
    include: Vec<String>,
    exclude: Vec<String>,
}

pub(crate) fn cmd_pack_refs(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let options = setup_pack_refs_options(args)?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let core_fsync = global_config_value("core.fsync")?;
    let core_fsync_method = global_config_value("core.fsyncMethod")?;
    let mut store = FileRefStore::new(&git_dir, format)
        .with_reference_fsync_config(core_fsync.as_deref(), core_fsync_method.as_deref())
        .with_reftable_lock_timeout_millis(reftable_lock_timeout_override()?);
    if store.uses_reftable()? {
        store = store.with_reftable_write_options(reftable_write_options(&common_git_dir)?);
        if options.auto && store.reftable_table_count()? <= 2 {
            return Ok(());
        }
        store.compact_reftable_stack().map_err(|err| {
            let locked = match &err {
                GitError::Io(message) => message.contains("File exists"),
                // Structured create errors surface lock contention by kind.
                GitError::IoKind {
                    kind: std::io::ErrorKind::AlreadyExists,
                    ..
                } => true,
                _ => false,
            };
            if locked {
                eprintln!("error: unable to compact stack: data is locked");
                GitError::Exit(1)
            } else if matches!(err, GitError::InvalidFormat(ref message) if message == "entry too large") {
                eprintln!("error: unable to compact stack: entry too large");
                GitError::Exit(1)
            } else {
                err
            }
        })?;
        trace_reference_fsync_counter(2);
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let timeout_millis = pack_refs_timeout_millis(&common_git_dir)?;
    store.pack_refs_selected_with_timeout(
        options.prune,
        options.auto,
        timeout_millis,
        |name| pack_refs_should_include(name, &options),
        |_, oid| match pack_refs_peeled_oid(&db, format, oid) {
            Ok(peeled) => Ok(PackRefDecision::Pack { peeled }),
            Err(GitError::NotFound(_)) => Ok(PackRefDecision::Skip),
            Err(err) => Err(err),
        },
    )?;
    Ok(())
}

/// git's `reftable_be_config`: validate the reftable writer options that the
/// backend reads at init. An out-of-bounds `reftable.blockSize` /
/// `reftable.restartInterval` is fatal before any work is done.
fn reftable_write_options(git_dir: &Path) -> Result<ReftableWriteOptions> {
    let config = read_repo_config(git_dir)?;
    let mut options = ReftableWriteOptions::default();
    if let Some(value) = config.get("reftable", None, "blockSize")
        && let Some(block_size) = parse_reftable_config_ulong(value)
    {
        if block_size > 16_777_215 {
            eprintln!("fatal: reftable block size cannot exceed 16MB");
            return Err(GitError::Exit(128));
        }
        options.block_size = block_size as u32;
    }
    if let Some(value) = config.get("reftable", None, "restartInterval")
        && let Some(restart_interval) = parse_reftable_config_ulong(value)
    {
        if restart_interval > 65_535 {
            eprintln!("fatal: reftable block size cannot exceed 65535");
            return Err(GitError::Exit(128));
        }
        options.restart_interval = restart_interval as u16;
    }
    if let Some(index_objects) = config.get_bool("reftable", None, "indexObjects") {
        options.index_objects = index_objects;
    }
    Ok(options)
}

/// Parse a config value as git's `git_config_ulong` does for the cases the
/// reftable options need: a plain integer with an optional `k`/`m`/`g` scaling
/// suffix.
fn parse_reftable_config_ulong(value: &str) -> Option<u64> {
    let value = value.trim();
    let (digits, scale) = match value.as_bytes().last() {
        Some(b'k') | Some(b'K') => (&value[..value.len() - 1], 1024),
        Some(b'm') | Some(b'M') => (&value[..value.len() - 1], 1024 * 1024),
        Some(b'g') | Some(b'G') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1),
    };
    digits.trim().parse::<u64>().ok().map(|n| n * scale)
}

fn setup_pack_refs_options(args: &[String]) -> Result<PackRefsOptions> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return pack_refs_help();
    }
    let parsed =
        parse_options(args, pack_refs_option_specs(), PACK_REFS_USAGE).map_err(cli_usage_error)?;
    if !parsed.positionals.is_empty() {
        return pack_refs_usage();
    }
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for option in &parsed.options {
        match option.long {
            Some("include") => {
                if matches!(option.name, OptionName::NegatedLong("include")) {
                    include.clear();
                } else if let ParsedValue::Str(pattern) = &option.value {
                    include.push(pattern.to_string());
                }
            }
            Some("exclude") => {
                if matches!(option.name, OptionName::NegatedLong("exclude")) {
                    exclude.clear();
                } else if let ParsedValue::Str(pattern) = &option.value {
                    exclude.push(pattern.to_string());
                }
            }
            _ => {}
        }
    }
    Ok(PackRefsOptions {
        all: parsed.last_bool("all", false),
        prune: parsed.last_bool("prune", true),
        auto: parsed.last_bool("auto", false),
        include,
        exclude,
    })
}

const PACK_REFS_USAGE: &[&str] =
    &["git pack-refs [--all] [--no-prune] [--auto] [--include <pattern>] [--exclude <pattern>]"];

fn pack_refs_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[
        opt_bool(None, Some("all"), OptFlags::NONE, "pack everything"),
        opt_bool(
            None,
            Some("prune"),
            OptFlags::NONE,
            "prune loose refs (default)",
        ),
        opt_bool(
            None,
            Some("auto"),
            OptFlags::NONE,
            "auto-pack refs as needed",
        ),
        opt_str(
            None,
            Some("include"),
            "<pattern>",
            OptFlags::NONE,
            "references to include",
        ),
        opt_str(
            None,
            Some("exclude"),
            "<pattern>",
            OptFlags::NONE,
            "references to exclude",
        ),
    ];
    SPECS
}

fn pack_refs_usage<T>() -> Result<T> {
    eprintln!(
        "usage: git pack-refs [--all] [--no-prune] [--auto] [--include <pattern>] [--exclude <pattern>]"
    );
    eprintln!();
    eprintln!("    --[no-]all            pack everything");
    eprintln!("    --[no-]prune          prune loose refs (default)");
    eprintln!("    --[no-]auto           auto-pack refs as needed");
    eprintln!("    --[no-]include <pattern>");
    eprintln!("                          references to include");
    eprintln!("    --[no-]exclude <pattern>");
    eprintln!("                          references to exclude");
    eprintln!();
    Err(GitError::Exit(129))
}

fn pack_refs_help<T>() -> Result<T> {
    println!(
        "usage: git pack-refs [--all] [--no-prune] [--auto] [--include <pattern>] [--exclude <pattern>]"
    );
    println!();
    println!("    --[no-]all            pack everything");
    println!("    --[no-]prune          prune loose refs (default)");
    println!("    --[no-]auto           auto-pack refs as needed");
    println!("    --[no-]include <pattern>");
    println!("                          references to include");
    println!("    --[no-]exclude <pattern>");
    println!("                          references to exclude");
    println!();
    Err(GitError::Exit(129))
}

fn pack_refs_should_include(name: &str, options: &PackRefsOptions) -> bool {
    if options
        .exclude
        .iter()
        .any(|pattern| refname_pattern_matches(pattern, name))
    {
        return false;
    }
    options.all
        || options
            .include
            .iter()
            .any(|pattern| refname_pattern_matches(pattern, name))
        || (options.include.is_empty() && name.starts_with("refs/tags/"))
}

fn pack_refs_timeout_millis(common_git_dir: &Path) -> Result<u64> {
    let config = read_repo_config(common_git_dir)?;
    Ok(config
        .get("core", None, "packedRefsTimeout")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1000))
}


#[derive(Debug)]
struct PruneOptions {
    dry_run: bool,
    verbose: bool,
    expire: i64,
    heads: Vec<String>,
}

pub(crate) fn cmd_prune(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let options = setup_prune_options(args)?;
    let repository = cli_session.open_repository()?;
    let git_dir = repository.git_dir();
    let common_git_dir = repository.common_dir();
    let format = repository.object_format();
    let db = repository.object_database();
    let mut roots = gc_prune::prune_roots(
        git_dir,
        common_git_dir,
        format,
        cli_session.replace_objects(),
        &options.heads,
    )?;
    roots.extend(gc_prune::prune_recent_object_roots(
        db,
        common_git_dir,
        format,
        options.expire,
    )?);
    roots.extend(gc_prune::prune_recent_hook_roots(common_git_dir, format)?);
    roots.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    roots.dedup();
    let reachable = collect_reachable_object_ids(db, format, roots.iter().copied())?;
    let mut candidates = Vec::new();
    for oid in prune_unreachable_loose(common_git_dir, format, roots.iter().copied(), false)? {
        if gc_prune::prune_object_is_expired(db, &oid, options.expire)? {
            candidates.push(oid);
        }
    }

    for oid in candidates {
        let object_type = db
            .loose()
            .read_header(&oid)?
            .map(|(object_type, _size)| object_type);
        if options.dry_run || options.verbose {
            let type_name = object_type.map(ObjectType::as_str).unwrap_or("unknown");
            println!("{oid} {type_name}");
        }
        if !options.dry_run {
            let path = db.loose().object_path(&oid)?;
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(GitError::Io(err.to_string())),
            }
        }
    }
    gc_prune::prune_shallow_file(
        common_git_dir,
        format,
        &reachable,
        options.dry_run,
        options.verbose,
    )?;
    gc_prune::prune_temporary_files(
        &common_git_dir.join("objects"),
        options.expire,
        options.dry_run,
        options.verbose,
    )?;
    gc_prune::prune_temporary_files(
        &common_git_dir.join("objects").join("pack"),
        options.expire,
        options.dry_run,
        options.verbose,
    )?;
    gc_prune::prune_packed_loose_objects(common_git_dir, format, options.dry_run)?;
    if !options.dry_run {
        gc_prune::prune_empty_loose_object_dirs(&common_git_dir.join("objects"))?;
    }
    Ok(())
}

fn setup_prune_options(args: &[String]) -> Result<PruneOptions> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return prune_help();
    }
    let parsed = parse_options(args, prune_option_specs(), PRUNE_USAGE).map_err(cli_usage_error)?;
    let mut expire = i64::MAX;
    for option in &parsed.options {
        if option.long != Some("expire") {
            continue;
        }
        if matches!(option.name, OptionName::NegatedLong("expire")) {
            expire = i64::MIN;
        } else if let ParsedValue::Str(value) = &option.value {
            expire = gc_prune::parse_prune_expire(value, "--expire")?;
        }
    }
    Ok(PruneOptions {
        dry_run: parsed.last_bool("dry-run", false),
        verbose: parsed.last_bool("verbose", false),
        expire,
        heads: parsed
            .positionals
            .iter()
            .map(|head| (*head).to_string())
            .collect(),
    })
}

const PRUNE_USAGE: &[&str] =
    &["git prune [-n] [-v] [--progress] [--expire <time>] [--] [<head>...]"];

fn prune_option_specs() -> &'static [sley_options::OptionSpec<'static>] {
    static SPECS: &[sley_options::OptionSpec<'static>] = &[
        opt_bool(
            Some('n'),
            Some("dry-run"),
            OptFlags::NONE,
            "do not remove, show only",
        ),
        opt_bool(
            Some('v'),
            Some("verbose"),
            OptFlags::NONE,
            "report pruned objects",
        ),
        opt_bool(None, Some("progress"), OptFlags::NONE, "show progress"),
        opt_str(
            None,
            Some("expire"),
            "<expiry-date>",
            OptFlags::NONE,
            "expire objects older than <time>",
        ),
        opt_bool(
            None,
            Some("exclude-promisor-objects"),
            OptFlags::NONE,
            "limit traversal to objects outside promisor packfiles",
        ),
    ];
    SPECS
}


/// `git prune -h`: the same usage text as `prune_usage`, but printed to stdout
/// (git's parse-options `-h` writes to stdout and exits 129).
fn prune_help<T>() -> Result<T> {
    println!("usage: git prune [-n] [-v] [--progress] [--expire <time>] [--] [<head>...]");
    println!();
    println!("    -n, --[no-]dry-run    do not remove, show only");
    println!("    -v, --[no-]verbose    report pruned objects");
    println!("    --[no-]progress       show progress");
    println!("    --[no-]expire <expiry-date>");
    println!("                          expire objects older than <time>");
    println!("    --[no-]exclude-promisor-objects");
    println!("                          limit traversal to objects outside promisor packfiles");
    println!();
    Err(GitError::Exit(129))
}


const MULTI_PACK_INDEX_USAGE: &str = "\
usage: git multi-pack-index [--object-dir <dir>] [--[no-]bitmap]
                            [--[no-]progress] <subcommand> [<options>]
";

pub(crate) fn cmd_multi_pack_index(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    // git accepts the shared options (`--object-dir`, `--[no-]progress`,
    // `--[no-]bitmap`) *before* the subcommand; collect them and prepend them to
    // the subcommand's own args so the per-subcommand parser sees them too.
    let mut global: Vec<String> = Vec::new();
    let mut iter = args.iter();
    let subcommand = loop {
        let Some(arg) = iter.next() else {
            // No subcommand ⇒ usage error (exit 129).
            eprint!("{MULTI_PACK_INDEX_USAGE}");
            return Err(GitError::Exit(129));
        };
        match arg.as_str() {
            "--object-dir" => {
                let Some(value) = iter.next() else {
                    return Err(GitError::Command("--object-dir requires a value".into()));
                };
                global.push("--object-dir".into());
                global.push(value.clone());
            }
            value
                if value.starts_with("--object-dir=")
                    || matches!(
                        value,
                        "--progress" | "--no-progress" | "--bitmap" | "--no-bitmap"
                    ) =>
            {
                global.push(value.to_string());
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                eprint!("{MULTI_PACK_INDEX_USAGE}");
                return Err(GitError::Exit(129));
            }
            other => break other.to_string(),
        }
    };
    let rest: Vec<String> = iter.cloned().collect();
    let combined: Vec<String> = global.into_iter().chain(rest).collect();
    match subcommand.as_str() {
        "compact" | "expire" | "repack" | "write" | "verify" => {
            let cwd = cli_session.cwd().to_path_buf();
            let git_dir = cli_session.git_dir()?;
            match subcommand.as_str() {
                "compact" => gc_midx::compact(&cwd, &git_dir, &combined),
                "expire" => gc_midx::expire(&cwd, &git_dir, &combined),
                "repack" => gc_midx::repack(&cwd, &git_dir, &combined),
                "write" => gc_midx::write(&cwd, &git_dir, &combined),
                _ => gc_midx::verify(&cwd, &git_dir, &combined),
            }
        }
        other => {
            eprintln!("error: unknown subcommand: `{other}'");
            eprint!("{MULTI_PACK_INDEX_USAGE}");
            Err(GitError::Exit(129))
        }
    }
}




/// A packfile considered by `git pack-redundant`, mirroring its `pack_list`.
struct RedundantPack {
    /// Filesystem path of the `.pack` (printed as upstream's `pack_name`).
    pack_path: PathBuf,
    /// Filesystem path of the `.idx` (printed as upstream's `idx_name`).
    idx_path: PathBuf,
    /// Working object set, reduced by alt-odb and stdin-ignore subtraction.
    remaining: Vec<ObjectId>,
    /// Objects only this pack holds among the local packs (filled by the
    /// pairwise comparison). Empty for single-pack repos, as upstream forces.
    unique: Vec<ObjectId>,
    /// Object count before any subtraction (upstream `all_objects_size`).
    all_objects_size: usize,
    local: bool,
}

/// `a := a - b` for two ascending OID lists (upstream
/// `llist_sorted_difference_inplace`).
fn oid_sorted_difference(a: &mut Vec<ObjectId>, b: &[ObjectId]) {
    if b.is_empty() || a.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(a.len());
    let mut j = 0;
    for oid in a.iter() {
        while j < b.len() && b[j].as_bytes() < oid.as_bytes() {
            j += 1;
        }
        if j < b.len() && b[j].as_bytes() == oid.as_bytes() {
            continue;
        }
        out.push(*oid);
    }
    *a = out;
}

/// |a ∩ b| for two ascending OID lists (upstream `sizeof_union`'s shared count).
fn oid_sorted_intersection_size(a: &[ObjectId], b: &[ObjectId]) -> usize {
    let (mut i, mut j, mut count) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].as_bytes().cmp(b[j].as_bytes()) {
            std::cmp::Ordering::Equal => {
                count += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    count
}

/// Read every `.idx`/`.pack` pair in `pack_dir`, returning each pack's
/// ascending OID list keyed by its filesystem paths.
fn pack_redundant_scan_dir(
    pack_dir: &Path,
    format: ObjectFormat,
    local: bool,
    into: &mut Vec<RedundantPack>,
) -> Result<()> {
    let Ok(entries) = fs::read_dir(pack_dir) else {
        return Ok(());
    };
    for entry in entries {
        let idx_path = entry?.path();
        if idx_path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        let pack_path = idx_path.with_extension("pack");
        if !pack_path.exists() {
            continue;
        }
        let index = PackIndex::parse(&fs::read(&idx_path)?, format)?;
        let mut oids: Vec<ObjectId> = index.entries.into_iter().map(|entry| entry.oid).collect();
        oids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        into.push(RedundantPack {
            pack_path,
            idx_path,
            all_objects_size: oids.len(),
            remaining: oids,
            unique: Vec::new(),
            local,
        });
    }
    Ok(())
}

/// `git pack-redundant`: report packs every one of whose objects also live in
/// some other pack (so the pack can be deleted without losing reachability).
/// Deprecated upstream; gated behind `--i-still-use-this`.
pub(crate) fn cmd_pack_redundant(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut load_all_packs = false;
    let mut verbose = false;
    let mut alt_odb = false;
    let mut i_still_use_this = false;
    let mut filenames: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                filenames.extend(iter.by_ref().cloned());
                break;
            }
            "--all" => load_all_packs = true,
            "--verbose" => verbose = true,
            "--alt-odb" => alt_odb = true,
            "--i-still-use-this" => i_still_use_this = true,
            other if other.starts_with('-') => {
                eprintln!(
                    "usage: git pack-redundant [--verbose] [--alt-odb] (--all | <pack-filename>...)"
                );
                return Err(GitError::Exit(129));
            }
            other => {
                filenames.push(other.to_string());
                filenames.extend(iter.by_ref().cloned());
                break;
            }
        }
    }

    if !i_still_use_this {
        eprintln!("'git pack-redundant' is nominated for removal.");
        eprintln!(
            "If you still use this command, here's what you can do:\n\n\
             - read https://git-scm.com/docs/BreakingChanges.html\n\
             - check if anyone has discussed this on the mailing\n  \
               list and if they came up with something that can\n  \
               help you: https://lore.kernel.org/git/?q=git%20pack-redundant\n\
             - send an email to <git@vger.kernel.org> to let us\n  \
               know that you still use this command and were unable\n  \
               to determine a suitable replacement\n"
        );
        eprintln!("fatal: refusing to run without --i-still-use-this");
        return Err(GitError::Exit(128));
    }

    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let objects_dir = repository_objects_dir(&common_git_dir);

    let mut packs: Vec<RedundantPack> = Vec::new();
    if load_all_packs {
        pack_redundant_scan_dir(&objects_dir.join("pack"), format, true, &mut packs)?;
        // Alternate packs are only loaded when they can matter — `--alt-odb`
        // subtracts them, `--verbose` reports their count (add_pack's veto).
        if (alt_odb || verbose)
            && let Ok(alternates) = fs::read_to_string(objects_dir.join("info/alternates"))
        {
            for raw in alternates.lines() {
                let raw = raw.trim();
                if raw.is_empty() || raw.starts_with('#') {
                    continue;
                }
                let path = PathBuf::from(raw);
                let alt = if path.is_absolute() {
                    path
                } else {
                    objects_dir.join(path)
                };
                pack_redundant_scan_dir(&alt.join("pack"), format, false, &mut packs)?;
            }
        }
    } else {
        // `<pack-filename>...`: match each against the local pack basenames.
        let mut local = Vec::new();
        pack_redundant_scan_dir(&objects_dir.join("pack"), format, true, &mut local)?;
        for filename in &filenames {
            if filename.len() < 40 {
                eprintln!("fatal: Bad pack filename: {filename}");
                return Err(GitError::Exit(128));
            }
            let Some(found) = local
                .iter()
                .position(|pack| pack.pack_path.to_string_lossy().contains(filename.as_str()))
            else {
                eprintln!("fatal: Filename {filename} not found in packed_git");
                return Err(GitError::Exit(128));
            };
            packs.push(local.remove(found));
        }
    }

    if !packs.iter().any(|pack| pack.local) {
        eprintln!("fatal: Zero packs found!");
        return Err(GitError::Exit(128));
    }

    let alt_count = packs.iter().filter(|pack| !pack.local).count();

    // all_objects: union of the local packs' objects, minus the alt-odb packs'.
    let mut all_objects: Vec<ObjectId> = Vec::new();
    for pack in packs.iter().filter(|pack| pack.local) {
        all_objects.extend(pack.remaining.iter().copied());
    }
    all_objects.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    all_objects.dedup();
    for pack in packs.iter().filter(|pack| !pack.local) {
        oid_sorted_difference(&mut all_objects, &pack.remaining);
    }

    // --alt-odb: drop objects already in an alternate pack from the local set.
    if alt_odb {
        let alt_remaining: Vec<Vec<ObjectId>> = packs
            .iter()
            .filter(|pack| !pack.local)
            .map(|pack| pack.remaining.clone())
            .collect();
        for pack in packs.iter_mut().filter(|pack| pack.local) {
            for alt in &alt_remaining {
                oid_sorted_difference(&mut pack.remaining, alt);
            }
        }
    }

    // Objects named on stdin are ignored (removed from consideration). Upstream
    // only reads stdin when it is not a terminal.
    if !io::stdin().is_terminal() {
        let mut ignore: Vec<ObjectId> = Vec::new();
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        for line in text.lines() {
            let token = line.trim();
            if token.is_empty() {
                continue;
            }
            let Ok(oid) = ObjectId::from_hex(format, token) else {
                eprintln!("fatal: Bad object ID on stdin: {line}");
                return Err(GitError::Exit(128));
            };
            ignore.push(oid);
        }
        ignore.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        ignore.dedup();
        oid_sorted_difference(&mut all_objects, &ignore);
        for pack in packs.iter_mut().filter(|pack| pack.local) {
            oid_sorted_difference(&mut pack.remaining, &ignore);
        }
    }

    // Pairwise comparison fills each local pack's `unique` set: objects in its
    // remaining set that no other local pack holds. A lone pack has none.
    let local_indices: Vec<usize> = (0..packs.len()).filter(|&i| packs[i].local).collect();
    if local_indices.len() > 1 {
        for &i in &local_indices {
            let mut unique = packs[i].remaining.clone();
            for &j in &local_indices {
                if i != j {
                    let other = packs[j].remaining.clone();
                    oid_sorted_difference(&mut unique, &other);
                }
            }
            packs[i].unique = unique;
        }
    }

    // minimize(): keep every pack with unique objects, then greedily cover the
    // rest with the non-unique packs holding the most still-missing objects.
    let mut min_indices: Vec<usize> = Vec::new();
    let mut non_unique: Vec<usize> = Vec::new();
    for &i in &local_indices {
        if packs[i].unique.is_empty() {
            non_unique.push(i);
        } else {
            min_indices.push(i);
        }
    }

    let mut missing = all_objects.clone();
    for &i in &min_indices {
        let remaining = packs[i].remaining.clone();
        oid_sorted_difference(&mut missing, &remaining);
    }

    if !missing.is_empty() {
        let mut unique_pack_objects = all_objects.clone();
        oid_sorted_difference(&mut unique_pack_objects, &missing);
        for &i in &non_unique {
            oid_sorted_difference(&mut packs[i].remaining, &unique_pack_objects);
        }

        loop {
            // Sort the survivors: most remaining objects first, ties broken by
            // larger original pack (upstream cmp_remaining_objects).
            non_unique.sort_by(|&a, &b| {
                packs[b]
                    .remaining
                    .len()
                    .cmp(&packs[a].remaining.len())
                    .then_with(|| packs[b].all_objects_size.cmp(&packs[a].all_objects_size))
            });
            match non_unique.first() {
                Some(&head) if !packs[head].remaining.is_empty() => {
                    let chosen = packs[head].remaining.clone();
                    non_unique.remove(0);
                    for &i in &non_unique {
                        if packs[i].remaining.is_empty() {
                            break;
                        }
                        oid_sorted_difference(&mut packs[i].remaining, &chosen);
                    }
                    min_indices.push(head);
                }
                _ => break,
            }
        }
    }

    // Redundant = local packs not in the minimal set, in discovery order.
    let min_set: HashSet<usize> = min_indices.iter().copied().collect();
    let redundant: Vec<usize> = local_indices
        .iter()
        .copied()
        .filter(|i| !min_set.contains(i))
        .collect();

    if verbose {
        eprintln!("There are {alt_count} packs available in alt-odbs.");
        eprintln!("The smallest (bytewise) set of packs is:");
        for &i in &min_indices {
            eprintln!(
                "\t{}",
                pack_redundant_display_path(&packs[i].pack_path, &cwd)
            );
        }
        let mut duplicates = 0usize;
        for (a, &ia) in min_indices.iter().enumerate() {
            for &ib in &min_indices[a + 1..] {
                duplicates +=
                    oid_sorted_intersection_size(&packs[ia].remaining, &packs[ib].remaining);
            }
        }
        let min_bytes: u64 = min_indices
            .iter()
            .map(|&i| pack_redundant_pack_bytes(&packs[i]))
            .sum();
        eprintln!(
            "containing {duplicates} duplicate objects with a total size of {}kb.",
            min_bytes / 1024
        );
        eprintln!(
            "A total of {} unique objects were considered.",
            all_objects.len()
        );
        eprintln!("Redundant packs (with indexes):");
    }

    let mut stdout = io::stdout().lock();
    for &i in &redundant {
        writeln!(
            stdout,
            "{}",
            pack_redundant_display_path(&packs[i].idx_path, &cwd)
        )?;
        writeln!(
            stdout,
            "{}",
            pack_redundant_display_path(&packs[i].pack_path, &cwd)
        )?;
    }
    stdout.flush()?;

    if verbose {
        let red_bytes: u64 = redundant
            .iter()
            .map(|&i| pack_redundant_pack_bytes(&packs[i]))
            .sum();
        eprintln!(
            "{}MB of redundant packs in total.",
            red_bytes / (1024 * 1024)
        );
    }

    Ok(())
}

/// Render a pack path relative to the working directory when possible, matching
/// git's `pack_name` (relative to the discovered object dir) so the output is
/// free of the absolute prefix — which, in the test suite, contains a space
/// that would otherwise split `xargs rm`.
fn pack_redundant_display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Combined `.pack` + `.idx` byte size for the verbose redundancy report.
fn pack_redundant_pack_bytes(pack: &RedundantPack) -> u64 {
    let pack_size = fs::metadata(&pack.pack_path).map(|m| m.len()).unwrap_or(0);
    let idx_size = fs::metadata(&pack.idx_path).map(|m| m.len()).unwrap_or(0);
    pack_size + idx_size
}

#[cfg(test)]
mod tests {
    use sley_gc::repack::resolve_cruft_pack_size;

    #[test]
    fn cruft_pack_size_uses_git_override_precedence() {
        assert_eq!(
            resolve_cruft_pack_size(Some(1), Some(10), Some(100)),
            Some(10),
            "--max-cruft-size overrides --max-pack-size and config"
        );
        assert_eq!(
            resolve_cruft_pack_size(Some(10), None, Some(1)),
            Some(10),
            "explicit --max-pack-size overrides config"
        );
        assert_eq!(
            resolve_cruft_pack_size(Some(10), Some(0), Some(1)),
            Some(10),
            "zero max-cruft-size inherits the general command limit"
        );
        assert_eq!(resolve_cruft_pack_size(None, None, Some(1)), Some(1));
    }
}

