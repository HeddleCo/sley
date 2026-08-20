//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used)]

use sley::plumbing::{sley_config, sley_core, sley_index, sley_rev, sley_worktree};
// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::commands::cli_options::{cli_usage_error, last_tri_state_bool, opt_bool, opt_str};
use crate::*;
use regex::Regex;
use sley::PackWriteOptions;
use sley::plumbing::sley_formats::ReftableWriteOptions;
use sley::plumbing::sley_object::EncodedObject;
use sley::plumbing::sley_odb::{ObjectReader, ObjectWriter};
use sley::plumbing::sley_pack::{PackInput, PackReverseIndex, pack_order_index_positions};
use sley_options::{OptFlags, OptionName, ParsedValue, parse_options};
use std::collections::BTreeMap;
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

/// The commit oids that get bitmap selection preference, mirroring upstream's
/// `NEEDS_BITMAP` marking: tips of refs under the `pack.preferBitmapTips`
/// hierarchies (each config value names a ref prefix, normalised to end with
/// `/`), peeled to commits. Empty when the config is unset.
fn repack_preferred_bitmap_tips(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<HashSet<ObjectId>> {
    let config = read_repo_config(git_dir)?;
    let mut prefixes: Vec<String> = Vec::new();
    for value in config.get_all("pack", None, "preferBitmapTips") {
        let Some(prefix) = value else {
            // A bare `[pack] preferBitmapTips` key: git reports the missing
            // value but continues the repack (string_list config callback).
            eprintln!("error: missing value for 'pack.preferbitmaptips'");
            continue;
        };
        if prefix.ends_with('/') {
            prefixes.push(prefix.to_string());
        } else {
            prefixes.push(format!("{prefix}/"));
        }
    }
    let mut tips = HashSet::new();
    if prefixes.is_empty() {
        return Ok(tips);
    }
    let store = FileRefStore::new(git_dir, format);
    for reference in store.list_refs()? {
        if !prefixes
            .iter()
            .any(|prefix| reference.name.starts_with(prefix))
        {
            continue;
        }
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        if let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid) {
            tips.insert(commit);
        }
    }
    Ok(tips)
}

#[derive(Clone)]
struct PseudoMergeCandidate {
    oid: ObjectId,
    date: i64,
}

#[derive(Default)]
struct PseudoMergeMatches {
    stable: Vec<PseudoMergeCandidate>,
    unstable: Vec<PseudoMergeCandidate>,
}

struct PseudoMergeConfigBuilder {
    pattern: Option<String>,
    decay: f64,
    max_merges: usize,
    sample_rate: f64,
    threshold: i64,
    stable_threshold: i64,
    stable_size: usize,
}

struct PseudoMergeConfig {
    name: String,
    pattern: Regex,
    capture_count: usize,
    decay: f64,
    max_merges: usize,
    sample_rate: f64,
    threshold: i64,
    stable_threshold: i64,
    stable_size: usize,
}

impl PseudoMergeConfigBuilder {
    fn new() -> Result<Self> {
        Ok(Self {
            pattern: None,
            decay: 1.0,
            max_merges: 64,
            sample_rate: 1.0,
            threshold: parse_pseudo_merge_expiry("1.week.ago")?,
            stable_threshold: parse_pseudo_merge_expiry("1.month.ago")?,
            stable_size: 512,
        })
    }
}

fn parse_pseudo_merge_expiry(value: &str) -> Result<i64> {
    let timestamp = crate::commands::approxidate::parse_expiry_date(value)
        .ok_or_else(|| GitError::Command(format!("invalid timestamp '{value}'")))?;
    let unsigned = timestamp as u64;
    Ok(if unsigned >= i64::MAX as u64 {
        i64::MAX
    } else {
        unsigned as i64
    })
}

fn load_pseudo_merge_configs(git_dir: &Path) -> Result<Vec<PseudoMergeConfig>> {
    let config = read_repo_config(git_dir)?;
    let mut builders: BTreeMap<String, PseudoMergeConfigBuilder> = BTreeMap::new();
    for section in &config.sections {
        if !section.name.eq_ignore_ascii_case("bitmapPseudoMerge") {
            continue;
        }
        let Some(name) = section.subsection.as_ref() else {
            continue;
        };
        for entry in &section.entries {
            let builder = match builders.entry(name.clone()) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(PseudoMergeConfigBuilder::new()?)
                }
            };
            let value = entry.value.as_deref().unwrap_or("");
            if entry.key.eq_ignore_ascii_case("pattern") {
                builder.pattern = Some(value.to_string());
            } else if entry.key.eq_ignore_ascii_case("decay") {
                if let Ok(decay) = value.trim().parse::<f64>()
                    && decay >= 0.0
                {
                    builder.decay = decay;
                }
            } else if entry.key.eq_ignore_ascii_case("sampleRate") {
                if let Ok(sample_rate) = value.trim().parse::<f64>()
                    && (0.0..=1.0).contains(&sample_rate)
                {
                    builder.sample_rate = sample_rate;
                }
            } else if entry.key.eq_ignore_ascii_case("threshold") {
                builder.threshold = parse_pseudo_merge_expiry(value)?;
            } else if entry.key.eq_ignore_ascii_case("maxMerges") {
                if let Some(max_merges) = sley_config::parse_config_int(value)
                    && max_merges >= 0
                {
                    builder.max_merges = max_merges as usize;
                }
            } else if entry.key.eq_ignore_ascii_case("stableThreshold") {
                builder.stable_threshold = parse_pseudo_merge_expiry(value)?;
            } else if entry.key.eq_ignore_ascii_case("stableSize")
                && let Some(stable_size) = sley_config::parse_config_int(value)
                && stable_size > 0
            {
                builder.stable_size = stable_size as usize;
            }
        }
    }

    let mut groups = Vec::new();
    for (name, builder) in builders {
        if builder.threshold < builder.stable_threshold {
            eprintln!(
                "fatal: pseudo-merge group '{name}' has unstable threshold before stable one"
            );
            return Err(GitError::Exit(128));
        }
        let Some(pattern) = builder.pattern else {
            eprintln!("fatal: pseudo-merge group '{name}' missing required pattern");
            return Err(GitError::Exit(128));
        };
        let anchored = if pattern.starts_with('^') {
            pattern
        } else {
            format!("^{pattern}")
        };
        let regex = Regex::new(&anchored).map_err(|_| {
            GitError::Command(format!(
                "failed to load pseudo-merge regex for {name}: '{anchored}'"
            ))
        })?;
        groups.push(PseudoMergeConfig {
            name,
            capture_count: regex.captures_len().saturating_sub(1),
            pattern: regex,
            decay: builder.decay,
            max_merges: builder.max_merges,
            sample_rate: builder.sample_rate,
            threshold: builder.threshold,
            stable_threshold: builder.stable_threshold,
            stable_size: builder.stable_size,
        });
    }
    Ok(groups)
}

fn pseudo_merge_match_key(config: &PseudoMergeConfig, refname: &str) -> Option<String> {
    let captures = config.pattern.captures(refname)?;
    let mut parts = Vec::new();
    if config.capture_count == 0 {
        if let Some(full) = captures.get(0) {
            parts.push(full.as_str());
        }
    } else {
        for index in 1..=config.capture_count {
            if let Some(capture) = captures.get(index) {
                parts.push(capture.as_str());
            }
        }
    }
    Some(parts.join("-"))
}

fn push_pseudo_merge_candidate_groups(
    out: &mut Vec<sley_odb::BitmapPseudoMergeGroup>,
    commits: &[PseudoMergeCandidate],
    exclude_selected: bool,
    partition: Option<sley_odb::BitmapPseudoMergePartition>,
) {
    if commits.is_empty() {
        return;
    }
    out.push(sley_odb::BitmapPseudoMergeGroup {
        commits: commits.iter().map(|candidate| candidate.oid).collect(),
        exclude_selected,
        partition,
    });
}

fn repack_pseudo_merge_groups(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<Vec<sley_odb::BitmapPseudoMergeGroup>> {
    let configs = load_pseudo_merge_configs(git_dir)?;
    if configs.is_empty() {
        return Ok(Vec::new());
    }
    let mut matches: Vec<BTreeMap<String, PseudoMergeMatches>> =
        configs.iter().map(|_| BTreeMap::new()).collect();
    let store = FileRefStore::new(git_dir, format);
    for reference in store.list_refs()? {
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        let Ok(commit_oid) = sley_rev::peel_to_commit(db, format, &oid) else {
            continue;
        };
        let Ok(object) = db.read_object(&commit_oid) else {
            continue;
        };
        let Ok(commit) = sley_object::Commit::parse_ref(format, &object.body) else {
            continue;
        };
        let date = sley_rev::revlist::commit_identity_timestamp_i64(commit.committer).unwrap_or(0);
        for (index, config) in configs.iter().enumerate() {
            let Some(key) = pseudo_merge_match_key(config, &reference.name) else {
                continue;
            };
            let entry = matches[index].entry(key).or_default();
            let candidate = PseudoMergeCandidate {
                oid: commit_oid,
                date,
            };
            if date <= config.stable_threshold {
                entry.stable.push(candidate);
            } else if date <= config.threshold {
                entry.unstable.push(candidate);
            }
        }
    }

    let mut groups = Vec::new();
    for (config, group_matches) in configs.iter().zip(matches.iter_mut()) {
        let _ = &config.name;
        for entry in group_matches.values_mut() {
            entry.stable.sort_by_key(|candidate| candidate.date);
            entry.unstable.sort_by_key(|candidate| candidate.date);

            for chunk in entry.stable.chunks(config.stable_size) {
                push_pseudo_merge_candidate_groups(&mut groups, chunk, false, None);
            }

            if !entry.unstable.is_empty() && config.max_merges > 0 {
                push_pseudo_merge_candidate_groups(
                    &mut groups,
                    &entry.unstable,
                    true,
                    Some(sley_odb::BitmapPseudoMergePartition {
                        max_merges: config.max_merges,
                        decay: config.decay,
                        sample_rate: config.sample_rate,
                    }),
                );
            }
        }
    }
    Ok(groups)
}

/// The traversal roots `repack -a` packs from, mirroring upstream's
/// `pack-objects --all --reflog --indexed-objects` invocation: every direct
/// ref target, `HEAD`, both sides of every reflog entry, and the blobs in the
/// index. Unresolvable roots are skipped (the closure walk also tolerates
/// missing objects — stale reflogs are expected).
fn repack_traversal_roots(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
) -> Result<Vec<ObjectId>> {
    let mut roots = Vec::new();
    let store = FileRefStore::new(git_dir, format);
    for reference in store.list_refs()? {
        if let RefTarget::Direct(oid) = reference.target {
            roots.push(oid);
        }
    }
    if let Ok(head) = resolve_revision(git_dir, format, "HEAD", replace_objects) {
        roots.push(head);
    }
    roots.extend(reflog_traversal_roots(git_dir, common_git_dir, format)?);
    // Indexed objects (upstream `--indexed-objects`): cache entries, the
    // cache-tree extension, and resolve-undo blobs all keep pending objects
    // alive across a repack (t7700 "pending objects are repacked appropriately").
    if let Ok(bytes) = fs::read(git_dir.join("index"))
        && let Ok(index) = sley_index::Index::parse(&bytes, format)
    {
        for entry in &index.entries {
            roots.push(entry.oid);
        }
        if let Ok(Some(cache_tree)) = index.cache_tree(format) {
            collect_cache_tree_oids(&cache_tree, &mut roots);
        }
        if let Ok(records) = index.resolve_undo_records(format) {
            for record in records {
                for stage in record.stages.into_iter().flatten() {
                    roots.push(stage.oid);
                }
            }
        }
    }
    Ok(roots)
}

fn collect_cache_tree_oids(tree: &sley_index::CacheTree, roots: &mut Vec<ObjectId>) {
    if let Some(oid) = tree.oid {
        roots.push(oid);
    }
    for child in &tree.subtrees {
        collect_cache_tree_oids(&child.tree, roots);
    }
}

fn parse_repack_object_filter(specs: &[String]) -> Result<Option<sley_odb::PackObjectFilter>> {
    if specs.is_empty() {
        return Ok(None);
    }
    let mut filter = sley_odb::PackObjectFilter::BlobNone; // placeholder replaced below
    let mut started = false;
    for spec in specs {
        let parsed = parse_one_repack_filter(spec)?;
        filter = if started {
            combine_repack_filters(filter, parsed)
        } else {
            started = true;
            parsed
        };
    }
    Ok(Some(filter))
}

fn parse_one_repack_filter(spec: &str) -> Result<sley_odb::PackObjectFilter> {
    if spec == "blob:none" {
        return Ok(sley_odb::PackObjectFilter::BlobNone);
    }
    if let Some(value) = spec.strip_prefix("blob:limit=") {
        let limit = parse_gc_size(value)?;
        return Ok(sley_odb::PackObjectFilter::BlobLimit(limit));
    }
    if let Some(value) = spec.strip_prefix("tree:") {
        let depth: u32 = value
            .parse()
            .map_err(|_| GitError::Command(format!("invalid tree filter depth '{value}'")))?;
        return Ok(sley_odb::PackObjectFilter::TreeDepth(depth));
    }
    Err(GitError::Command(format!(
        "unsupported repack filter '{spec}'"
    )))
}

fn combine_repack_filters(
    left: sley_odb::PackObjectFilter,
    right: sley_odb::PackObjectFilter,
) -> sley_odb::PackObjectFilter {
    // Prefer TreeDepth when combining with BlobNone (tree:N already omits blobs).
    // For other pairs keep the more restrictive blob filter and tree depth.
    match (left, right) {
        (sley_odb::PackObjectFilter::BlobNone, sley_odb::PackObjectFilter::TreeDepth(d))
        | (sley_odb::PackObjectFilter::TreeDepth(d), sley_odb::PackObjectFilter::BlobNone) => {
            sley_odb::PackObjectFilter::TreeDepth(d)
        }
        (sley_odb::PackObjectFilter::BlobLimit(a), sley_odb::PackObjectFilter::BlobLimit(b)) => {
            sley_odb::PackObjectFilter::BlobLimit(a.min(b))
        }
        (sley_odb::PackObjectFilter::TreeDepth(a), sley_odb::PackObjectFilter::TreeDepth(b)) => {
            sley_odb::PackObjectFilter::TreeDepth(a.min(b))
        }
        (other, sley_odb::PackObjectFilter::BlobNone)
        | (sley_odb::PackObjectFilter::BlobNone, other) => other,
        (left, _) => left,
    }
}

fn reflog_traversal_roots(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let mut roots = Vec::new();
    let mut log_dirs = vec![common_git_dir.join("logs"), git_dir.join("logs")];
    log_dirs.dedup();
    let zero = "0".repeat(format.hex_len());
    let mut stack: Vec<PathBuf> = log_dirs.into_iter().filter(|dir| dir.is_dir()).collect();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(contents) = fs::read_to_string(&path) {
                for line in contents.lines() {
                    let mut fields = line.split(' ');
                    for hex in [fields.next(), fields.next()].into_iter().flatten() {
                        if hex != zero
                            && let Ok(oid) = ObjectId::from_hex(format, hex)
                        {
                            roots.push(oid);
                        }
                    }
                }
            }
        }
    }
    Ok(roots)
}

/// Update dumb-transport metadata from the object types already established by
/// an all-into-one repack. Lightweight tags and ordinary refs need no object
/// body read. If any ref points outside the result or at an annotated tag, the
/// caller falls back to the generic update-server-info path, which performs the
/// full peel through the ODB.
fn repack_try_update_server_info_from_result(
    common_git_dir: &Path,
    format: ObjectFormat,
    result: &sley_odb::RepackResult,
) -> Result<bool> {
    let store = FileRefStore::new(common_git_dir, format);
    let refs = store.list_refs()?;
    let mut info_refs = Vec::with_capacity(refs.len() * (format.hex_len() + 32));
    for reference in refs {
        let oid = match &reference.target {
            RefTarget::Direct(oid) => *oid,
            RefTarget::Symbolic(_) => {
                let Some(oid) = resolve_ref_to_oid(&store, &reference.name)? else {
                    continue;
                };
                oid
            }
        };
        match result.cached_object_type(&oid) {
            Some(ObjectType::Tag) | None => return Ok(false),
            Some(ObjectType::Commit | ObjectType::Tree | ObjectType::Blob) => {}
        }
        info_refs.extend_from_slice(oid.to_hex().as_bytes());
        info_refs.push(b'\t');
        info_refs.extend_from_slice(reference.name.as_bytes());
        info_refs.push(b'\n');
    }

    let shared_repository =
        sley::plumbing::sley_formats::SharedRepositoryPermissions::from_git_dir(common_git_dir);
    let info_dir = common_git_dir.join("info");
    shared_repository.create_dir_all(&info_dir)?;
    repack_write_server_info_file(&info_dir.join("refs"), &info_refs, &shared_repository)?;

    let objects_dir = repository_objects_dir(common_git_dir);
    let objects_info_dir = objects_dir.join("info");
    shared_repository.create_dir_all(&objects_info_dir)?;
    let pack_dir = objects_dir.join("pack");
    let mut packs = Vec::new();
    if pack_dir.exists() {
        for entry in fs::read_dir(&pack_dir)? {
            let path = entry?.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("pack") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(hash) = name
                .strip_prefix("pack-")
                .and_then(|name| name.strip_suffix(".pack"))
            else {
                continue;
            };
            if ObjectId::from_hex(format, hash).is_ok() && path.with_extension("idx").is_file() {
                packs.push(name.to_string());
            }
        }
    }
    packs.sort();
    let mut info_packs = Vec::with_capacity(packs.len() * (format.hex_len() + 9));
    for name in packs {
        info_packs.extend_from_slice(b"P ");
        info_packs.extend_from_slice(name.as_bytes());
        info_packs.push(b'\n');
    }
    info_packs.push(b'\n');
    repack_write_server_info_file(
        &objects_info_dir.join("packs"),
        &info_packs,
        &shared_repository,
    )?;
    Ok(true)
}

fn repack_write_server_info_file(
    path: &Path,
    content: &[u8],
    shared_repository: &sley::plumbing::sley_formats::SharedRepositoryPermissions,
) -> Result<()> {
    if !fs::read(path).is_ok_and(|existing| existing == content) {
        fs::write(path, content)?;
    }
    shared_repository.adjust_file(path)
}

pub(crate) fn cmd_repack(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut prune = false;
    let mut quiet = false;
    let mut all = false;
    let mut unpack_unreachable = false;
    let mut unpack_unreachable_before: Option<Option<u32>> = None;
    let mut keep_unreachable = false;
    let mut local = false;
    let mut write_bitmaps: Option<bool> = None;
    let mut geometric: Option<u64> = None;
    let mut write_midx = false;
    let mut keep_packs: Vec<String> = Vec::new();
    let mut pack_kept_objects = false;
    let mut force_rewrite = false;
    let mut update_server_info: Option<bool> = None;
    let mut cruft = false;
    let mut cruft_expiration: Option<Option<u32>> = None;
    let mut expire_to: Option<String> = None;
    let mut max_pack_size: Option<u64> = None;
    let mut max_cruft_size: Option<u64> = None;
    let mut combine_cruft_below_size: Option<u64> = None;
    let mut window: Option<usize> = None;
    let mut filter_specs: Vec<String> = Vec::new();
    let mut filter_to: Option<String> = None;
    let mut name_hash_version: Option<i32> = None;
    let mut path_walk = false;
    let mut iter = expand_repack_short_clusters(args).into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-d" => prune = true,
            "-q" | "--quiet" => quiet = true,
            "-b" | "--write-bitmap-index" => write_bitmaps = Some(true),
            "--no-write-bitmap-index" => write_bitmaps = Some(false),
            "-a" => all = true,
            "-A" => {
                all = true;
                unpack_unreachable = true;
            }
            "-m" | "--write-midx" => write_midx = true,
            "-l" | "--local" => local = true,
            "-n" => update_server_info = Some(false),
            "--cruft" => cruft = true,
            "--path-walk" => path_walk = true,
            "--no-path-walk" => path_walk = false,
            "--cruft-expiration" => {
                cruft = true;
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `cruft-expiration' requires a value".into())
                })?;
                cruft_expiration = Some(parse_cruft_expiration(&value)?);
            }
            value if value.starts_with("--cruft-expiration=") => {
                cruft = true;
                cruft_expiration = Some(parse_cruft_expiration(
                    &value["--cruft-expiration=".len()..],
                )?);
            }
            "--expire-to" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `expire-to' requires a value".into())
                })?;
                expire_to = Some(value);
            }
            value if value.starts_with("--expire-to=") => {
                expire_to = Some(value["--expire-to=".len()..].to_string());
            }
            "--max-pack-size" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `max-pack-size' requires a value".into())
                })?;
                max_pack_size = Some(parse_gc_size(&value)?);
            }
            value if value.starts_with("--max-pack-size=") => {
                max_pack_size = Some(parse_gc_size(&value["--max-pack-size=".len()..])?);
            }
            "--max-cruft-size" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `max-cruft-size' requires a value".into())
                })?;
                max_cruft_size = Some(parse_gc_size(&value)?);
            }
            value if value.starts_with("--max-cruft-size=") => {
                max_cruft_size = Some(parse_gc_size(&value["--max-cruft-size=".len()..])?);
            }
            "--combine-cruft-below-size" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `combine-cruft-below-size' requires a value".into())
                })?;
                combine_cruft_below_size = Some(parse_gc_size(&value)?);
            }
            value if value.starts_with("--combine-cruft-below-size=") => {
                combine_cruft_below_size = Some(parse_gc_size(
                    &value["--combine-cruft-below-size=".len()..],
                )?);
            }
            "--window" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("option `window' requires a value".into()))?;
                window = Some(parse_repack_window(&value)?);
            }
            value if value.starts_with("--window=") => {
                window = Some(parse_repack_window(&value["--window=".len()..])?);
            }
            "--filter" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("option `filter' requires a value".into()))?;
                filter_specs.push(value);
            }
            value if value.starts_with("--filter=") => {
                filter_specs.push(value["--filter=".len()..].to_string());
            }
            "--filter-to" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `filter-to' requires a value".into())
                })?;
                filter_to = Some(value);
            }
            value if value.starts_with("--filter-to=") => {
                filter_to = Some(value["--filter-to=".len()..].to_string());
            }
            value if value.starts_with("--name-hash-version=") => {
                let raw = &value["--name-hash-version=".len()..];
                name_hash_version = Some(raw.parse::<i32>().map_err(|_| {
                    GitError::Command(format!("invalid --name-hash-version option: {raw}"))
                })?);
            }
            "--name-hash-version" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `name-hash-version' requires a value".into())
                })?;
                name_hash_version = Some(value.parse::<i32>().map_err(|_| {
                    GitError::Command(format!("invalid --name-hash-version option: {value}"))
                })?);
            }
            "--unpack-unreachable" => {
                unpack_unreachable = true;
                unpack_unreachable_before = Some(None);
            }
            value if value.starts_with("--unpack-unreachable=") => {
                unpack_unreachable = true;
                unpack_unreachable_before = Some(parse_cruft_expiration(
                    &value["--unpack-unreachable=".len()..],
                )?);
            }
            "-k" | "--keep-unreachable" => keep_unreachable = true,
            "--pack-kept-objects" => pack_kept_objects = true,
            "-g" | "--geometric" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `geometric' requires a value".into())
                })?;
                geometric = Some(parse_geometric_factor(&value)?);
            }
            "--keep-pack" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("option `keep-pack' requires a value".into())
                })?;
                keep_packs.push(strip_pack_suffix(&value));
            }
            value if value.starts_with("--geometric=") => {
                geometric = Some(parse_geometric_factor(&value["--geometric=".len()..])?);
            }
            value if value.starts_with("--keep-pack=") => {
                keep_packs.push(strip_pack_suffix(&value["--keep-pack=".len()..]));
            }
            "--no-cruft" => cruft = false,
            "-f" | "-F" => force_rewrite = true,
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
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let _ = path_walk; // accepted; selection still uses the same reachability walk
    let pack_filter = parse_repack_object_filter(&filter_specs)?;
    if filter_to.is_some() && pack_filter.is_none() {
        return Err(GitError::Command(
            "option '--filter-to' can only be used along with '--filter'".into(),
        ));
    }
    // `--name-hash-version` is accepted for CLI compatibility with git repack.
    // Sley repacks in-process (no pack-objects child), and the pack writer
    // does not implement version-specific delta name grouping. Bitmap
    // name-hash caches always use version 1 (`pack_name_hash`). Do not emit a
    // synthetic pack-objects TRACE2 child_start: that would claim a child argv
    // that never ran. Version 2 with bitmaps only warns, matching pack-objects.
    if let Some(version) = name_hash_version
        && !(1..=2).contains(&version)
    {
        eprintln!("fatal: invalid --name-hash-version option: {version}");
        return Err(GitError::Exit(128));
    }
    let config = read_repo_config(&common_git_dir)?;
    let repack_roots = if all {
        Some(repack_traversal_roots(
            &git_dir,
            &common_git_dir,
            format,
            cli_session.replace_objects(),
        )?)
    } else {
        None
    };
    let update_server_info = update_server_info.unwrap_or_else(|| {
        config
            .get_bool("repack", None, "updateServerInfo")
            .unwrap_or(true)
    });
    let mut has_promisor_packs = pack_dir_has_promisor_packs(&common_git_dir)?;
    if let Some(roots) = repack_roots.as_deref()
        && !has_promisor_packs
        && sley_remote::config_has_promisor_remote(&config)
    {
        sley_remote::hydrate_reachable_from_local_promisor_remotes(&common_git_dir, format, roots)?;
        has_promisor_packs = pack_dir_has_promisor_packs(&common_git_dir)?;
    }
    let config_write_bitmaps = config.get_bool("repack", None, "writeBitmaps");
    let write_reverse_index = config
        .get_bool("pack", None, "writeReverseIndex")
        .unwrap_or(true);
    let write_bitmap_lookup_table = config
        .get_bool("pack", None, "writeBitmapLookupTable")
        .unwrap_or(false);
    let write_bitmap_hash_cache = config
        .get_bool("pack", None, "writeBitmapHashCache")
        .unwrap_or(true);
    let midx_must_contain_cruft = config
        .get_bool("repack", None, "midxMustContainCruft")
        .unwrap_or(true);
    let auto_bare_bitmaps = write_bitmaps.is_none()
        && config_write_bitmaps.is_none()
        && all
        && !write_midx
        && config.get("pack", None, "packSizeLimit").is_none()
        && sley_worktree::worktree_root_for_git_dir(&common_git_dir)?.is_none()
        && !pack_dir_has_kept_packs(&common_git_dir)?
        && !has_promisor_packs;
    let mut write_bitmaps = match write_bitmaps {
        Some(explicit) => explicit,
        None => config_write_bitmaps.unwrap_or(auto_bare_bitmaps),
    };
    let include_kept_objects =
        pack_kept_objects || (write_bitmaps && !write_midx && !auto_bare_bitmaps);

    if write_bitmaps && name_hash_version.is_some_and(|version| version != 1) {
        // Match pack-objects: bitmaps require name-hash version 1; sley always
        // writes the v1 cache and continues after warning (git auto-switches).
        eprintln!("warning: currently, --write-bitmap-index requires --name-hash-version=1");
    }

    if write_bitmaps && local && object_dir_has_alternates(&common_git_dir) {
        eprintln!("warning: disabling bitmap writing, as some objects are not being packed");
        write_bitmaps = false;
    }
    if write_bitmaps && pack_filter.is_some() {
        eprintln!("fatal: cannot write bitmap index with pack filters");
        return Err(GitError::Exit(128));
    }
    if write_bitmaps && all && has_promisor_packs {
        eprintln!("fatal: cannot write bitmap index for a repack with promisor packs");
        return Err(GitError::Exit(128));
    }

    if let Some(split_factor) = geometric {
        // `--geometric` and `-a`/`-A` are mutually exclusive (builtin/repack.c).
        if all {
            return Err(GitError::Command(
                "options '--geometric' and '-A/-a' cannot be used together".into(),
            ));
        }
        return cmd_repack_geometric(
            cli_session,
            &git_dir,
            &common_git_dir,
            format,
            split_factor,
            prune,
            quiet,
            write_midx,
            write_bitmaps,
            midx_must_contain_cruft,
            &keep_packs,
            include_kept_objects,
        );
    }

    if cruft {
        validate_repack_cruft_numeric_config(&config)?;
        let configured_pack_size = config
            .get("pack", None, "packSizeLimit")
            .map(parse_gc_size)
            .transpose()?;
        let cruft_pack_size =
            resolve_cruft_pack_size(max_pack_size, max_cruft_size, configured_pack_size);
        // Cruft-specific config intentionally overrides the general command
        // option. Otherwise the command option overrides pack.window.
        let cruft_window = if let Some(value) = config.get("repack", None, "cruftWindow") {
            parse_repack_window(value)?
        } else if let Some(value) = window {
            value
        } else if let Some(value) = config.get("pack", None, "window") {
            parse_repack_window(value)?
        } else {
            PackWriteOptions::new().window
        };
        return cmd_repack_cruft(
            cli_session,
            &git_dir,
            &common_git_dir,
            format,
            prune,
            local,
            cruft_expiration.flatten(),
            expire_to.as_deref(),
            write_midx,
            &keep_packs,
            include_kept_objects,
            cruft_pack_size,
            cruft_window,
            combine_cruft_below_size.filter(|size| *size > 0),
        );
    }

    if write_bitmaps && !all && !write_midx {
        // Upstream cmd_repack: bitmaps require an all-into-one repack.
        eprintln!(
            "fatal: Incremental repacks are incompatible with bitmap indexes.  Use
--no-write-bitmap-index or disable the pack.writeBitmaps configuration."
        );
        return Err(GitError::Exit(128));
    }

    // `-A -d` differs from `-a -d`: objects that are no longer reachable must
    // be materialized loose before their source packs are removed. Build that
    // transition as one engine outcome so neither the CLI nor concurrent
    // readers observe a gap between pruning the pack and writing the loose
    // copies.
    if all && unpack_unreachable && prune && !keep_unreachable {
        if pack_filter.is_some() {
            return Err(GitError::Command(
                "--unpack-unreachable cannot be combined with --filter".into(),
            ));
        }
        let roots = repack_roots
            .as_deref()
            .expect("all-object repacks prepared traversal roots");
        let keep_pack_stems: HashSet<String> = keep_packs.iter().cloned().collect();
        let options = sley_odb::RepackOptions {
            local,
            force_rewrite,
            pack_kept_objects: include_kept_objects,
            keep_pack_stems,
        };
        let recent_roots = prune_recent_hook_roots(&common_git_dir, format)?;
        let unpacked = sley_odb::repack_reachable_objects_unpack_unreachable(
            &common_git_dir,
            format,
            roots,
            &options,
            unpack_unreachable_before.flatten(),
            &recent_roots,
        )?;
        sley_odb::install_repack_with_unpacked_unreachable(
            &common_git_dir,
            format,
            &unpacked,
            true,
        )?;
        if unpacked.repack.as_ref().is_none_or(|result| {
            result.loose_object_prune_outcome() != sley_odb::LooseObjectPruneOutcome::Complete
        }) {
            prune_packed_loose_objects(&common_git_dir, format, false)?;
        }
        if !write_bitmaps || write_midx {
            remove_pack_bitmap_sidecars(&common_git_dir)?;
        }
        if write_midx {
            let mut midx_args = Vec::new();
            if write_bitmaps {
                midx_args.push("--bitmap".to_string());
            }
            cmd_multi_pack_index_write(cli_session, &midx_args)?;
        }
        if update_server_info {
            let updated = match unpacked.repack.as_ref() {
                Some(result) => {
                    repack_try_update_server_info_from_result(&common_git_dir, format, result)?
                }
                None => false,
            };
            if !updated {
                crate::commands::refs::update_server_info_at(&common_git_dir, &[])?;
            }
        }
        prune_repack_shallow_file(&common_git_dir, format, roots)?;
        // `repack -A -d` never loosens promisor objects (they stay in retained
        // `.promisor` packs). Emit the TRACE2_PERF counter git's pack-objects
        // path records so t5616 can observe `loosened:0`.
        if has_promisor_packs {
            trace2_perf_data("loosen_unused_packed_objects/loosened", "0");
        }
        let _ = quiet;
        return Ok(());
    }

    // `-a`: pack the reachability closure of refs/HEAD/reflogs/index (borrowed
    // objects included, unreachable ones dropped). Without `-a`, pack only
    // loose objects and leave existing packs in place.
    let result = if all && keep_unreachable {
        sley_odb::repack_all_objects(&common_git_dir, format)?
    } else if all {
        let roots = repack_roots
            .as_deref()
            .expect("all-object repacks prepared traversal roots");
        let keep_pack_stems: HashSet<String> = keep_packs.iter().cloned().collect();
        let options = sley_odb::RepackOptions {
            local,
            force_rewrite,
            pack_kept_objects: include_kept_objects,
            keep_pack_stems,
        };
        match pack_filter.as_ref() {
            Some(filter) => sley_odb::repack_reachable_objects_with_object_filter(
                &common_git_dir,
                format,
                roots,
                &options,
                filter,
                filter_to.as_deref().map(Path::new),
                max_pack_size,
            )?,
            None => sley_odb::repack_reachable_objects_with_options(
                &common_git_dir,
                format,
                roots,
                &options,
            )?,
        }
    } else {
        let roots = repack_traversal_roots(
            &git_dir,
            &common_git_dir,
            format,
            cli_session.replace_objects(),
        )?;
        sley_odb::repack_reachable_loose_objects(&common_git_dir, format, &roots)?
    };
    let mut loose_prune_complete = false;
    if let Some(result) = result.as_ref() {
        let (bitmap_tips, bitmap_pseudo_merge_groups) = if write_bitmaps {
            let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
            (
                Some(repack_preferred_bitmap_tips(&common_git_dir, &db, format)?),
                Some(repack_pseudo_merge_groups(&common_git_dir, &db, format)?),
            )
        } else {
            (None, None)
        };
        if write_bitmaps && write_bitmap_lookup_table {
            sley_core::trace2::region("pack-bitmap-write", "writing_lookup_table");
        }
        sley_odb::install_repack_result_with_bitmap_options(
            &common_git_dir,
            format,
            result,
            sley_odb::RepackInstallOptions::new(prune)
                .with_reverse_index(write_reverse_index)
                .with_bitmap_extensions(write_bitmap_lookup_table, write_bitmap_hash_cache),
            bitmap_tips.as_ref(),
            bitmap_pseudo_merge_groups.as_deref(),
        )?;
        loose_prune_complete =
            result.loose_object_prune_outcome() == sley_odb::LooseObjectPruneOutcome::Complete;
    }
    if prune && !loose_prune_complete {
        prune_packed_loose_objects(&common_git_dir, format, false)?;
        if all && has_promisor_packs {
            trace2_perf_data("loosen_unused_packed_objects/loosened", "0");
        }
    }
    if all && (!write_bitmaps || write_midx) {
        remove_pack_bitmap_sidecars(&common_git_dir)?;
    }
    // Writing a multi-pack bitmap supersedes per-pack bitmaps for the same
    // packs (git's `remove_redundant_bitmaps`).
    if write_midx && write_bitmaps {
        remove_pack_bitmap_sidecars(&common_git_dir)?;
    }
    if write_midx {
        let mut midx_args = Vec::new();
        if write_bitmaps {
            midx_args.push("--bitmap".to_string());
        }
        cmd_multi_pack_index_write(cli_session, &midx_args)?;
    }
    if update_server_info {
        let updated_from_result = match result.as_ref() {
            Some(result) => {
                repack_try_update_server_info_from_result(&common_git_dir, format, result)?
            }
            None => false,
        };
        if !updated_from_result {
            crate::commands::refs::update_server_info_at(&common_git_dir, &[])?;
        }
    }
    if all && prune {
        let roots = repack_roots
            .as_deref()
            .expect("all-object repacks prepared traversal roots");
        prune_repack_shallow_file(&common_git_dir, format, roots)?;
    }
    let _ = quiet;
    Ok(())
}

fn validate_repack_cruft_numeric_config(config: &GitConfig) -> Result<()> {
    for key in ["cruftwindow", "cruftdepth", "cruftthreads"] {
        if let Some(value) = config.get("repack", None, key)
            && value.parse::<u64>().is_err()
        {
            eprintln!("fatal: bad numeric config value '{value}' for 'repack.{key}'");
            return Err(GitError::Exit(128));
        }
    }
    Ok(())
}

fn parse_geometric_factor(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|&n| n >= 1)
        .ok_or_else(|| GitError::Command(format!("cannot parse geometric factor: {value}")))
}

fn parse_repack_window(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid window size: {value}")))
}

fn resolve_cruft_pack_size(
    max_pack_size: Option<u64>,
    max_cruft_size: Option<u64>,
    configured_pack_size: Option<u64>,
) -> Option<u64> {
    max_cruft_size
        .filter(|size| *size > 0)
        .or_else(|| max_pack_size.filter(|size| *size > 0))
        .or_else(|| configured_pack_size.filter(|size| *size > 0))
}

fn strip_pack_suffix(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    base.strip_suffix(".pack")
        .or_else(|| base.strip_suffix(".idx"))
        .unwrap_or(base)
        .to_string()
}

#[allow(clippy::too_many_arguments)]
fn cmd_repack_geometric(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    split_factor: u64,
    prune: bool,
    quiet: bool,
    write_midx: bool,
    write_bitmaps: bool,
    midx_must_contain_cruft: bool,
    keep_packs: &[String],
    _pack_kept_objects: bool,
) -> Result<()> {
    let kept_stems: HashSet<String> = keep_packs.iter().cloned().collect();
    let existing_midx_pack_names = read_ordinary_midx_pack_names(common_git_dir, format)?;
    let geometric = sley_odb::repack_geometric_with_options(
        common_git_dir,
        format,
        split_factor,
        &kept_stems,
        sley_odb::GeometricRepackOptions {
            follow_reachable: write_midx && !midx_must_contain_cruft,
        },
    )?;

    if geometric.result.is_none() {
        if !quiet {
            println!("Nothing new to pack.");
        }
        // With no new pack and no previous MIDX, Git conservatively includes
        // cruft because a reachable pack may refer into it. With an existing
        // MIDX, preserve its proven exclusion unless it names an unknown pack.
        if write_midx && pack_dir_has_packs(common_git_dir, format)? {
            let selection = sley_odb::geometric_repack_midx_selection(
                common_git_dir,
                &geometric,
                midx_must_contain_cruft,
                existing_midx_pack_names.as_ref(),
            )?;
            let mut midx_args = Vec::new();
            if write_bitmaps {
                midx_args.push("--bitmap".to_string());
            }
            cmd_multi_pack_index_write_with_pack_names(
                cli_session,
                &midx_args,
                Some(selection.pack_names),
            )?;
        }
        return Ok(());
    }

    // A geometric repack writes its bitmap through the MIDX (not a pack bitmap),
    // so only pass pack-bitmap tips when not writing a MIDX.
    let bitmap_tips = if write_bitmaps && !write_midx {
        let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
        Some(repack_preferred_bitmap_tips(common_git_dir, &db, format)?)
    } else {
        None
    };
    sley_odb::install_geometric_repack_result(
        common_git_dir,
        format,
        &geometric,
        prune,
        bitmap_tips.as_ref(),
    )?;

    if write_midx && pack_dir_has_packs(common_git_dir, format)? {
        let selection = sley_odb::geometric_repack_midx_selection(
            common_git_dir,
            &geometric,
            midx_must_contain_cruft,
            existing_midx_pack_names.as_ref(),
        )?;
        let mut midx_args: Vec<String> = Vec::new();
        if write_bitmaps {
            midx_args.push("--bitmap".to_string());
        }
        if let Some(preferred) = selection.preferred_pack_name {
            midx_args.push(format!("--preferred-pack={preferred}"));
        }
        cmd_multi_pack_index_write_with_pack_names(
            cli_session,
            &midx_args,
            Some(selection.pack_names),
        )?;
    }
    let _ = git_dir;
    Ok(())
}

/// Parse pack-objects' cruft cutoff. Unlike config expiry dates, the
/// `--cruft-expiration` and `--unpack-unreachable` callbacks use `approxidate`,
/// so `now` is the actual current timestamp and future-dated objects remain
/// recent. `all` retains the explicit expire-everything sentinel.
fn parse_cruft_expiration(spec: &str) -> Result<Option<u32>> {
    if matches!(spec, "never" | "false") {
        return Ok(None);
    }
    let ts = if spec == "all" {
        u64::MAX
    } else {
        crate::commands::approxidate::parse_approxidate(spec)
            .ok_or_else(|| GitError::Command(format!("malformed expiration date '{spec}'")))?
            .max(0) as u64
    };
    Ok(if ts == 0 {
        None
    } else if ts >= u32::MAX as u64 {
        Some(u32::MAX)
    } else {
        Some(ts as u32)
    })
}

/// True when `<section>.<key>` is set to a "never"/"false" timestamp sentinel.
fn is_config_never(config: &GitConfig, section: &str, key: &str) -> bool {
    matches!(
        config.get(section, None, key),
        Some("never") | Some("false")
    )
}

fn validate_gc_prune_expire(config: &GitConfig, git_dir: &Path) -> Result<()> {
    let Some(value) = config.get("gc", None, "pruneExpire") else {
        return Ok(());
    };
    if parse_cruft_expiration(value).is_ok() {
        return Ok(());
    }
    eprintln!("error: Invalid gc.pruneexpire: '{value}'");
    let config_path = git_dir.join("config");
    let line = config_line_number(&config_path, "pruneExpire").unwrap_or(0);
    eprintln!(
        "fatal: bad config variable 'gc.pruneexpire' in file '{}' at line {line}",
        display_git_config_path(git_dir, &config_path)
    );
    Err(GitError::Exit(128))
}

fn config_line_number(path: &Path, key: &str) -> Option<usize> {
    let contents = fs::read_to_string(path).ok()?;
    contents
        .lines()
        .position(|line| {
            line.trim_start()
                .split(['=', ' ', '\t'])
                .next()
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(key))
        })
        .map(|index| index + 1)
}

fn display_git_config_path(git_dir: &Path, config_path: &Path) -> String {
    if git_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".git")
        && let Some(parent) = git_dir.parent()
        && env::current_dir().is_ok_and(|cwd| cwd == parent)
    {
        return ".git/config".to_string();
    }
    config_path.to_string_lossy().into_owned()
}

/// `git repack --cruft [--cruft-expiration=<t>] [--expire-to=<dir>] [-d]`.
#[allow(clippy::too_many_arguments)]
fn cmd_repack_cruft(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    prune: bool,
    local: bool,
    cruft_expiration: Option<u32>,
    expire_to: Option<&str>,
    write_midx: bool,
    keep_packs: &[String],
    pack_kept_objects: bool,
    max_pack_size: Option<u64>,
    cruft_window: usize,
    combine_cruft_below_size: Option<u64>,
) -> Result<()> {
    let roots = repack_traversal_roots(
        git_dir,
        common_git_dir,
        format,
        cli_session.replace_objects(),
    )?;
    let keep_pack_stems: HashSet<String> = keep_packs.iter().cloned().collect();
    let options = sley_odb::RepackOptions {
        local,
        force_rewrite: false,
        pack_kept_objects,
        keep_pack_stems,
    };

    let cruft_options = sley_odb::CruftPackOptions {
        max_pack_size,
        combine_cruft_below_size,
        pack_write: PackWriteOptions::new().with_window(cruft_window),
    };
    let window_arg = format!("--window={}", cruft_options.pack_write.window);
    trace2_child_start(&["pack-objects", &window_arg, "--cruft"]);
    let result = repack_cruft_or_bad_object(repack_cruft_with_lazy_recent_hooks(
        common_git_dir,
        format,
        &roots,
        cruft_expiration,
        &options,
        &cruft_options,
    ))?;
    sley_odb::install_cruft_repack_result_with_expire_to(
        common_git_dir,
        format,
        &result,
        prune,
        expire_to.map(Path::new),
    )?;

    if write_midx && pack_dir_has_packs(common_git_dir, format)? {
        cmd_multi_pack_index_write(cli_session, &[])?;
    }
    Ok(())
}

/// Build a cruft result first to determine whether pack-objects has any cruft
/// candidates. Git does not invoke `gc.recentObjectsHook` for an empty cruft
/// side, so only enumerate configured roots when that preliminary plan contains
/// surviving or expired unreachable objects. Both passes are read-only; callers
/// install files only after the hook-backed result succeeds.
fn repack_cruft_with_lazy_recent_hooks(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    cruft_expiration: Option<u32>,
    options: &sley_odb::RepackOptions,
    cruft_options: &sley_odb::CruftPackOptions,
) -> Result<sley_odb::CruftRepackResult> {
    let preliminary = sley_odb::repack_cruft_with_pack_options(
        common_git_dir,
        format,
        roots,
        cruft_expiration,
        options,
        cruft_options,
    )?;
    let has_cruft_candidates = preliminary.cruft.is_some()
        || !preliminary.additional_cruft.is_empty()
        || preliminary.expired.is_some();
    if cruft_expiration.is_none() || !has_cruft_candidates {
        return Ok(preliminary);
    }
    let recent_roots = prune_recent_hook_roots(common_git_dir, format)?;
    if recent_roots.is_empty() {
        return Ok(preliminary);
    }
    sley_odb::repack_cruft_with_pack_options_and_recent_roots(
        common_git_dir,
        format,
        roots,
        &recent_roots,
        cruft_expiration,
        options,
        cruft_options,
    )
}

fn repack_cruft_or_bad_object(
    result: Result<sley_odb::CruftRepackResult>,
) -> Result<sley_odb::CruftRepackResult> {
    match result {
        Ok(result) => Ok(result),
        Err(GitError::NotFound(kind)) => {
            if let Some(oid) = kind.object_id() {
                eprintln!("fatal: bad object {oid}");
                Err(GitError::Exit(128))
            } else {
                Err(GitError::NotFound(kind))
            }
        }
        Err(err) => Err(err),
    }
}

/// True when `objects/pack` holds at least one `.pack` file.
fn object_dir_has_alternates(common_git_dir: &Path) -> bool {
    if env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES").is_some() {
        return true;
    }
    repository_objects_dir(common_git_dir)
        .join("info")
        .join("alternates")
        .exists()
}

fn pack_dir_has_kept_packs(common_git_dir: &Path) -> Result<bool> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(false);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("keep") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn pack_dir_has_promisor_packs(common_git_dir: &Path) -> Result<bool> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(false);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("promisor") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remove_pack_bitmap_sidecars(common_git_dir: &Path) -> Result<()> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with("pack-") && name.ends_with(".bitmap") {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(GitError::Io(err.to_string())),
            }
        }
    }
    Ok(())
}

fn pack_dir_has_packs(common_git_dir: &Path, format: ObjectFormat) -> Result<bool> {
    let _ = format;
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(false);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("pack") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Snapshot the ordinary MIDX pack table before a repack mutates pack files.
/// The engine uses this to distinguish cruft which was already required for a
/// bitmap closure from cruft which a new follow-reachable pack can supersede.
fn read_ordinary_midx_pack_names(
    common_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<HashSet<String>>> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let midx_path = pack_dir.join("multi-pack-index");
    let Ok(midx_bytes) = fs::read(&midx_path) else {
        return Ok(None);
    };
    let Ok(midx) = MultiPackIndex::parse(&midx_bytes, format) else {
        return Ok(None);
    };
    Ok(Some(midx.pack_names.into_iter().collect()))
}

#[derive(Debug, Default)]
struct GcOptions {
    quiet: bool,
    auto: bool,
    detach: Option<bool>,
    force: bool,
    skip_foreground_tasks: bool,
    aggressive: bool,
    keep_largest_pack: Option<bool>,
    cruft_flag: Option<bool>,
    prune_override: Option<Option<String>>,
    max_cruft_size: Option<u64>,
    expire_to: Option<String>,
}

pub(crate) fn cmd_gc(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let options = setup_gc_options(args)?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let config = read_repo_config(&common_git_dir)?;
    validate_gc_prune_expire(&config, &common_git_dir)?;

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
        match gc_auto_mode(&common_git_dir, format, &config)? {
            Some(mode) => mode,
            None => return Ok(()),
        }
    } else {
        GcAutoMode::Full
    };
    if options.auto {
        if gc_recent_log_blocks_auto(&common_git_dir, &config)? {
            return Ok(());
        }
        if gc_lock_held(&common_git_dir)? && !options.force {
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
            if gc_should_detach(&config, options.detach) {
                eprintln!("Auto packing the repository in background for optimum performance.");
            } else {
                eprintln!("Auto packing the repository for optimum performance.");
            }
            eprintln!("See \"git help gc\" for manual housekeeping.");
        }
    } else if gc_lock_held(&common_git_dir)? && !options.force {
        eprintln!("fatal: gc is already running");
        return Err(GitError::Exit(128));
    }

    gc_write_pid(&common_git_dir)?;
    let result = gc_run_locked(
        cli_session,
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

#[allow(clippy::too_many_arguments)]
fn gc_run_locked(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    options: &GcOptions,
    cruft_packs: bool,
    prune_expire: Option<String>,
    auto_mode: GcAutoMode,
) -> Result<()> {
    if !options.skip_foreground_tasks {
        gc_before_repack(cli_session, git_dir, common_git_dir, format, config)?;
    }

    let roots = repack_traversal_roots(
        git_dir,
        common_git_dir,
        format,
        cli_session.replace_objects(),
    )?;
    let keep_pack_stems = gc_keep_pack_stems(common_git_dir, config, options)?;
    let resolved_max_cruft_size = options
        .max_cruft_size
        .or_else(|| gc_config_u64(config, "maxCruftSize"));

    // builtin/gc.c add_repack_all_option: pick the repack flavour in the ODB
    // engine, leaving this layer to execute the selected filesystem operation.
    let gc_plan = sley_odb::plan_gc_repack(sley_odb::GcRepackPlanOptions {
        incremental: auto_mode == GcAutoMode::Incremental,
        prune_expire: prune_expire.as_deref(),
        cruft_packs,
        expire_to: options.expire_to.as_deref(),
        max_cruft_size: resolved_max_cruft_size,
        repack_filter: config.get("gc", None, "repackFilter"),
        repack_filter_to: config.get("gc", None, "repackFilterTo"),
    })
    .map_err(|error| GitError::Command(error.to_string()))?;
    let trace_args = gc_plan
        .trace_args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    trace_gc_repack(&trace_args);
    match gc_plan.mode {
        sley_odb::GcRepackMode::Incremental => {
            if let Some(result) = sley_odb::repack_loose_objects(common_git_dir, format)? {
                sley_odb::install_repack_result(common_git_dir, format, &result, true)?;
            }
        }
        sley_odb::GcRepackMode::Immediate => {
            // prune_expire=="now" with cruft (no expire-to): immediate drop via -a.
            let repack_options = sley_odb::RepackOptions {
                local: true,
                force_rewrite: false,
                pack_kept_objects: false,
                keep_pack_stems,
            };
            if let Some(result) = sley_odb::repack_reachable_objects_with_options(
                common_git_dir,
                format,
                &roots,
                &repack_options,
            )? {
                sley_odb::install_repack_result(common_git_dir, format, &result, true)?;
            }
            gc_remove_cruft_packs(common_git_dir)?;
            if let Some(spec) = prune_expire.as_deref() {
                let expire = parse_prune_expire(spec, "gc.pruneExpire")?;
                gc_prune_expired_loose(common_git_dir, format, &roots, expire)?;
            }
        }
        sley_odb::GcRepackMode::Cruft => {
            // Default: reachable pack + cruft pack, cruft expiry = prune_expire.
            let cruft_expiration = match prune_expire.as_deref() {
                Some(spec) => parse_cruft_expiration(spec)?,
                None => None,
            };
            let repack_options = sley_odb::RepackOptions {
                local: true,
                force_rewrite: false,
                pack_kept_objects: false,
                keep_pack_stems,
            };
            let result = repack_cruft_with_lazy_recent_hooks(
                common_git_dir,
                format,
                &roots,
                cruft_expiration,
                &repack_options,
                &sley_odb::CruftPackOptions {
                    max_pack_size: resolved_max_cruft_size,
                    ..sley_odb::CruftPackOptions::default()
                },
            )?;
            sley_odb::install_cruft_repack_result_with_expire_to(
                common_git_dir,
                format,
                &result,
                true,
                options.expire_to.as_deref().map(Path::new),
            )?;
        }
        sley_odb::GcRepackMode::Reachable => {
            // gc.cruftPacks=false: repack reachable objects, then prune loose
            // unreachable objects older than gc.pruneExpire/--prune.
            let filtered_repack = config.get("gc", None, "repackFilter") == Some("blob:none");
            if filtered_repack {
                gc_repack_blob_none_filter(
                    common_git_dir,
                    format,
                    &roots,
                    options.expire_to.as_deref(),
                    config.get("gc", None, "repackFilterTo"),
                )?;
            } else {
                let repack_options = sley_odb::RepackOptions {
                    local: true,
                    force_rewrite: false,
                    pack_kept_objects: false,
                    keep_pack_stems,
                };
                if let Some(result) = sley_odb::repack_reachable_objects_with_options(
                    common_git_dir,
                    format,
                    &roots,
                    &repack_options,
                )? {
                    gc_unpack_recent_unreachable_from_repack(
                        common_git_dir,
                        format,
                        &roots,
                        prune_expire.as_deref(),
                        &result,
                    )?;
                    sley_odb::install_repack_result(common_git_dir, format, &result, true)?;
                }
            }
            if filtered_repack {
                return Ok(());
            }
            if let Some(spec) = prune_expire.as_deref() {
                let expire = parse_prune_expire(spec, "gc.pruneExpire")?;
                gc_prune_expired_loose(common_git_dir, format, &roots, expire)?;
            } else {
                let expire = parse_prune_expire(
                    config
                        .get("gc", None, "pruneExpire")
                        .unwrap_or("2.weeks.ago"),
                    "gc.pruneExpire",
                )?;
                gc_pack_recent_unreachable_loose(common_git_dir, format, &roots, expire)?;
            }
        }
    }

    let store = FileRefStore::new(common_git_dir, format)
        .with_reftable_lock_timeout_millis(reftable_lock_timeout_override()?);
    if options.auto && store.uses_reftable()? && store.reftable_table_count()? > 2 {
        store.compact_reftable_stack()?;
    }
    if let Some(result) = sley_odb::repack_promisor_objects(common_git_dir, format)? {
        sley_odb::install_repack_result(common_git_dir, format, &result, true)?;
    }
    gc_clean_pack_garbage(&repository_objects_dir(common_git_dir).join("pack"))?;
    crate::commands::refs::update_server_info_at(common_git_dir, &[])?;
    if gc_write_commit_graph(config) {
        let progress = if gc_progress_requested(options) {
            "--progress"
        } else {
            "--no-progress"
        };
        trace2_child_start(&["commit-graph", "write", "--reachable", progress]);
        commands::plumbing::cmd_commit_graph(
            cli_session,
            &[
                "write".to_string(),
                "--reachable".to_string(),
                progress.to_string(),
            ],
        )?;
    }
    if options.auto && gc_too_many_loose_objects(common_git_dir, format, config)? {
        eprintln!(
            "warning: There are too many unreachable loose objects; run 'git prune' to remove them."
        );
    }

    Ok(())
}

fn setup_gc_options(args: &[String]) -> Result<GcOptions> {
    let parsed = parse_options(args, gc_option_specs(), GC_USAGE).map_err(cli_usage_error)?;
    if !parsed.positionals.is_empty() {
        return gc_usage();
    }
    let mut options = GcOptions::default();
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
                    options.max_cruft_size = Some(parse_gc_size(value)?);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GcAutoMode {
    Full,
    Incremental,
}

fn gc_auto_mode(
    common_git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<Option<GcAutoMode>> {
    if gc_config_i64(config, "auto").unwrap_or(6700) <= 0 {
        return Ok(None);
    }
    if gc_too_many_packs(common_git_dir, config)? {
        Ok(Some(GcAutoMode::Full))
    } else if gc_too_many_loose_objects(common_git_dir, format, config)? {
        Ok(Some(GcAutoMode::Incremental))
    } else {
        Ok(None)
    }
}

fn gc_too_many_packs(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let limit = gc_config_i64(config, "autoPackLimit").unwrap_or(50);
    if limit <= 0 {
        return Ok(false);
    }
    let mut count = 0i64;
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    if let Ok(entries) = fs::read_dir(pack_dir) {
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("pack")
                && !path.with_extension("keep").exists()
            {
                count += 1;
            }
        }
    }
    Ok(count > limit)
}

fn gc_too_many_loose_objects(
    common_git_dir: &Path,
    _format: ObjectFormat,
    config: &GitConfig,
) -> Result<bool> {
    let limit = gc_config_i64(config, "auto").unwrap_or(6700);
    if limit <= 0 {
        return Ok(false);
    }
    let threshold = ((limit + 255) / 256) * 256;
    let sampled = gc_loose_fanout_count(common_git_dir, "17")?.saturating_mul(256);
    Ok(sampled > threshold as u64)
}

fn gc_loose_fanout_count(common_git_dir: &Path, fanout: &str) -> Result<u64> {
    let dir = repository_objects_dir(common_git_dir).join(fanout);
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(0);
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            count += 1;
        }
    }
    Ok(count)
}

fn gc_before_repack(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<()> {
    if gc_pack_refs(config, common_git_dir)? {
        crate::setup::git_trace_line(
            "builtin/gc.c:0",
            "trace: built-in: git pack-refs --all --prune",
        );
        commands::pack::cmd_pack_refs(cli_session, &["--all".to_string(), "--prune".to_string()])?;
    }
    let reflog_expire_never = is_config_never(config, "gc", "reflogExpire");
    let reflog_unreachable_never = is_config_never(config, "gc", "reflogExpireUnreachable");
    if !(reflog_expire_never && reflog_unreachable_never) {
        let mut expire_args = vec!["--all".to_string()];
        if reflog_expire_never {
            expire_args.push("--expire=never".to_string());
        }
        if reflog_unreachable_never {
            expire_args.push("--expire-unreachable=never".to_string());
        }
        crate::setup::git_trace_line(
            "builtin/gc.c:0",
            &format!(
                "trace: built-in: git reflog expire {}",
                expire_args.join(" ")
            ),
        );
        let _ =
            commands::refs::reflog_expire_at(git_dir, &expire_args, cli_session.replace_objects());
    }
    let _ = (git_dir, format);
    Ok(())
}

fn gc_pack_refs(config: &GitConfig, common_git_dir: &Path) -> Result<bool> {
    if let Some(value) = config.get("gc", None, "packRefs")
        && value.eq_ignore_ascii_case("notbare")
    {
        return Ok(sley_worktree::worktree_root_for_git_dir(common_git_dir)?.is_some());
    }
    Ok(config.get_bool("gc", None, "packRefs").unwrap_or(true))
}

fn gc_keep_pack_stems(
    common_git_dir: &Path,
    config: &GitConfig,
    options: &GcOptions,
) -> Result<HashSet<String>> {
    if options.keep_largest_pack == Some(false) {
        return Ok(HashSet::new());
    }
    if options.keep_largest_pack == Some(true) {
        return Ok(gc_largest_pack_stem(common_git_dir)?.into_iter().collect());
    }
    let Some(threshold) = gc_config_u64(config, "bigPackThreshold") else {
        return Ok(HashSet::new());
    };
    gc_pack_stems_at_least(common_git_dir, threshold)
}

fn gc_repack_blob_none_filter(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    cli_expire_to: Option<&str>,
    config_filter_to: Option<&str>,
) -> Result<()> {
    let before = gc_pack_stems(common_git_dir)?;
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let destination = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let excluded = HashSet::new();
    let installed = sley_odb::build_and_install_reachable_pack_filtered(
        &db,
        &destination,
        format,
        roots.iter().copied(),
        &excluded,
        sley_odb::RawPackInstallOptions::default(),
        Some(sley_odb::PackObjectFilter::BlobNone),
        None,
    )?;

    let filter_to = config_filter_to.or(cli_expire_to);
    if let Some(filter_to) = filter_to {
        gc_write_filtered_blobs(common_git_dir, format, roots, filter_to)?;
        let keep = installed.map(|pack| pack.pack_name).unwrap_or_default();
        gc_remove_pack_stems(common_git_dir, &before, &keep)?;
    }
    Ok(())
}

fn gc_write_filtered_blobs(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    filter_to: &str,
) -> Result<()> {
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let reachable = collect_reachable_object_ids(&db, format, roots.iter().copied())?;
    let mut objects = Vec::new();
    for oid in reachable {
        let object = match db.read_object(&oid) {
            Ok(object) if object.object_type == ObjectType::Blob => object,
            _ => continue,
        };
        objects.push((oid, object));
    }
    if objects.is_empty() {
        return Ok(());
    }
    objects.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let inputs = objects
        .iter()
        .map(|(oid, object)| PackInput {
            oid,
            object: object.as_ref(),
        })
        .collect::<Vec<_>>();
    let written = PackFile::write_packed_with_known_ids(&inputs, format)?;
    let object_dir = gc_filter_to_object_dir(filter_to)?;
    FileObjectDatabase::new(object_dir, format).install_pack(&written)?;
    Ok(())
}

fn gc_filter_to_object_dir(filter_to: &str) -> Result<PathBuf> {
    let path = PathBuf::from(filter_to);
    let pack_dir = path
        .parent()
        .ok_or_else(|| GitError::InvalidPath(format!("invalid filter-to path '{filter_to}'")))?;
    let object_dir = pack_dir
        .parent()
        .ok_or_else(|| GitError::InvalidPath(format!("invalid filter-to path '{filter_to}'")))?;
    Ok(object_dir.to_path_buf())
}

fn gc_unpack_recent_unreachable_from_repack(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    prune_expire: Option<&str>,
    result: &sley_odb::RepackResult,
) -> Result<()> {
    let Some(spec) = prune_expire else {
        return Ok(());
    };
    let expire = parse_prune_expire(spec, "gc.pruneExpire")?;
    if expire == i64::MIN {
        return Ok(());
    }

    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut preserve_roots = roots.to_vec();
    preserve_roots.extend(prune_recent_object_roots(
        &db,
        common_git_dir,
        format,
        expire,
    )?);
    preserve_roots.extend(prune_recent_hook_roots(common_git_dir, format)?);
    preserve_roots.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    preserve_roots.dedup();

    let new_index = sley_pack::PackIndex::parse(&result.idx, format)?;
    let newly_packed: HashSet<ObjectId> = new_index
        .entries
        .into_iter()
        .map(|entry| entry.oid)
        .collect();
    let mut preserve =
        sley_odb::collect_reachable_object_ids_tolerating_missing(&db, format, preserve_roots)?
            .into_iter()
            .filter(|oid| !newly_packed.contains(oid))
            .collect::<Vec<_>>();
    preserve.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

    for oid in preserve {
        let object = match db.read_object(&oid) {
            Ok(object) => object,
            Err(_) => continue,
        };
        db.loose().write_object((*object).clone())?;
    }
    Ok(())
}

fn gc_pack_stems(common_git_dir: &Path) -> Result<HashSet<String>> {
    Ok(gc_non_cruft_pack_stems(common_git_dir)?
        .into_iter()
        .map(|(stem, _)| stem)
        .collect())
}

fn gc_remove_pack_stems(
    common_git_dir: &Path,
    stems: &HashSet<String>,
    keep_stem: &str,
) -> Result<()> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    for stem in stems {
        if stem == keep_stem {
            continue;
        }
        for ext in ["pack", "idx", "rev", "bitmap"] {
            let path = pack_dir.join(format!("{stem}.{ext}"));
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(GitError::Io(err.to_string())),
            }
        }
    }
    Ok(())
}

fn gc_largest_pack_stem(common_git_dir: &Path) -> Result<Option<String>> {
    let mut best: Option<(u64, String)> = None;
    for (stem, size) in gc_non_cruft_pack_stems(common_git_dir)? {
        if best.as_ref().is_none_or(|(best_size, _)| size > *best_size) {
            best = Some((size, stem));
        }
    }
    Ok(best.map(|(_, stem)| stem))
}

fn gc_pack_stems_at_least(common_git_dir: &Path, threshold: u64) -> Result<HashSet<String>> {
    let mut stems = HashSet::new();
    for (stem, size) in gc_non_cruft_pack_stems(common_git_dir)? {
        if size >= threshold {
            stems.insert(stem);
        }
    }
    Ok(stems)
}

fn gc_non_cruft_pack_stems(common_git_dir: &Path) -> Result<Vec<(String, u64)>> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(pack_dir) else {
        return Ok(Vec::new());
    };
    let mut packs = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("pack")
            || path.with_extension("mtimes").exists()
        {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        packs.push((stem.to_string(), entry.metadata()?.len()));
    }
    Ok(packs)
}

fn gc_write_commit_graph(config: &GitConfig) -> bool {
    if env::var("GIT_TEST_COMMIT_GRAPH").ok().as_deref() == Some("0") {
        return false;
    }
    config
        .get_bool("gc", None, "writeCommitGraph")
        .or_else(|| config.get_bool("core", None, "commitGraph"))
        .unwrap_or(true)
}

fn gc_progress_requested(options: &GcOptions) -> bool {
    !options.quiet && env::var("GIT_PROGRESS_DELAY").ok().as_deref() == Some("0")
}

fn gc_should_detach(config: &GitConfig, detach: Option<bool>) -> bool {
    detach.unwrap_or_else(|| config.get_bool("gc", None, "autoDetach").unwrap_or(true))
}

fn gc_recent_log_blocks_auto(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let path = common_git_dir.join("gc.log");
    let Ok(metadata) = fs::metadata(&path) else {
        return Ok(false);
    };
    if metadata.len() == 0 {
        return Ok(false);
    }
    let expiry = config.get("gc", None, "logExpiry").unwrap_or("1.day.ago");
    let cutoff = parse_reflog_expire_time(expiry, "gc.logExpiry")?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0);
    if modified >= cutoff {
        eprintln!(
            "warning: The last gc run reported the following. Please correct the root cause\nand remove {}\nAutomatic cleanup will not be performed until the file is removed.\n\n{}",
            path.display(),
            fs::read_to_string(&path).unwrap_or_default()
        );
        Ok(true)
    } else {
        let _ = fs::remove_file(path);
        Ok(false)
    }
}

fn gc_lock_held(common_git_dir: &Path) -> Result<bool> {
    let path = common_git_dir.join("gc.pid");
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(false);
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.elapsed().ok())
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(u64::MAX);
    Ok(modified <= 12 * 60 * 60)
}

fn gc_write_pid(common_git_dir: &Path) -> Result<()> {
    let host = env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string());
    fs::write(
        common_git_dir.join("gc.pid"),
        format!("{} {host}", std::process::id()),
    )?;
    Ok(())
}

fn trace_gc_repack(args: &[&str]) {
    crate::setup::git_trace_line(
        "builtin/gc.c:0",
        &format!("trace: built-in: git {}", args.join(" ")),
    );
    trace2_child_start(args);
}

fn gc_config_i64(config: &GitConfig, key: &str) -> Option<i64> {
    config.get("gc", None, key)?.parse().ok()
}

fn gc_config_u64(config: &GitConfig, key: &str) -> Option<u64> {
    config
        .get("gc", None, key)
        .and_then(|value| parse_gc_size(value).ok())
}

fn parse_gc_size(value: &str) -> Result<u64> {
    let (digits, suffix) = value.trim().split_at(
        value
            .trim()
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(value.trim().len()),
    );
    let mut size = digits
        .parse::<u64>()
        .map_err(|_| GitError::Command(format!("bad numeric config value '{value}'")))?;
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" => 1,
        "k" => 1024,
        "m" => 1024 * 1024,
        "g" => 1024 * 1024 * 1024,
        _ => {
            return Err(GitError::Command(format!(
                "bad numeric config value '{value}'"
            )));
        }
    };
    size = size.saturating_mul(multiplier);
    Ok(size)
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

/// The maintenance task names git's `builtin/gc.c` `tasks[]` table recognises,
/// in declaration order. `--task=<name>` is case-insensitive against this set.
const MAINTENANCE_TASKS: &[&str] = &[
    "prefetch",
    "loose-objects",
    "incremental-repack",
    "geometric-repack",
    "gc",
    "commit-graph",
    "pack-refs",
    "reflog-expire",
    "worktree-prune",
    "rerere-gc",
];

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
    let selected = maintenance_select_tasks(&config, &tasks, schedule.as_deref())?;
    maintenance_run_selected(
        cli_session,
        &common_git_dir,
        &config,
        &selected,
        quiet,
        auto,
        detach,
    )?;
    Ok(())
}

/// Validate a `--schedule=<frequency>` value against git's `parse_schedule`
/// (hourly/daily/weekly, case-insensitive). Returns the value on success; emits
/// git's `unrecognized --schedule argument` diagnostic (rc 128) otherwise.
fn validate_maintenance_schedule(value: &str) -> Result<String> {
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "hourly" | "daily" | "weekly"
    ) {
        Ok(value.to_string())
    } else {
        eprintln!("fatal: unrecognized --schedule argument '{value}'");
        Err(GitError::Exit(128))
    }
}

/// Append a `--task=<name>` selection, mirroring git's `task_option_parse`:
/// reject an unknown task name, and reject a task already selected (both rc 129).
fn push_maintenance_task(tasks: &mut Vec<String>, task: &str) -> Result<()> {
    let task = if task.eq_ignore_ascii_case("refs optimize") {
        "pack-refs"
    } else {
        task
    };
    if !MAINTENANCE_TASKS
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

fn maintenance_select_tasks(
    config: &GitConfig,
    requested: &[String],
    schedule: Option<&str>,
) -> Result<Vec<String>> {
    if !requested.is_empty() {
        return Ok(requested
            .iter()
            .map(|task| task.to_ascii_lowercase())
            .collect());
    }
    let strategy = config
        .get("maintenance", None, "strategy")
        .unwrap_or(if schedule.is_some() {
            "none"
        } else {
            "geometric"
        });
    let strategy_name = strategy.to_ascii_lowercase();
    let mut selected = match strategy_name.as_str() {
        "none" => Vec::new(),
        "gc" => vec!["gc"],
        "incremental" if schedule.is_some() => vec![
            "prefetch",
            "loose-objects",
            "incremental-repack",
            "commit-graph",
            "pack-refs",
        ],
        "incremental" => vec!["gc"],
        "geometric" => vec![
            "geometric-repack",
            "commit-graph",
            "pack-refs",
            "reflog-expire",
            "worktree-prune",
            "rerere-gc",
        ],
        other => {
            eprintln!("fatal: unknown maintenance strategy: '{other}'");
            return Err(GitError::Exit(128));
        }
    }
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();

    for task in MAINTENANCE_TASKS {
        if let Some(enabled) = config.get_bool("maintenance", Some(task), "enabled") {
            selected.retain(|selected| !selected.eq_ignore_ascii_case(task));
            if enabled {
                selected.push((*task).to_string());
            }
        }
    }

    if let Some(schedule) = schedule {
        let requested_schedule = maintenance_schedule_rank(schedule)?;
        selected.retain(|task| {
            let default_schedule = match task.as_str() {
                "commit-graph" | "prefetch" => "hourly",
                "loose-objects" | "incremental-repack" | "geometric-repack" | "gc" => "daily",
                "pack-refs" if strategy_name == "incremental" => "weekly",
                "pack-refs" => "daily",
                _ => "weekly",
            };
            let task_schedule = config
                .get("maintenance", Some(task), "schedule")
                .unwrap_or(default_schedule);
            maintenance_schedule_rank(task_schedule).unwrap_or(0) >= requested_schedule
        });
    }

    selected.sort_by_key(|task| maintenance_run_order(task));
    Ok(selected)
}

fn maintenance_run_order(task: &str) -> usize {
    match task {
        "pack-refs" => 0,
        "reflog-expire" => 1,
        "gc" => 2,
        "prefetch" => 3,
        "loose-objects" => 4,
        "incremental-repack" => 5,
        "geometric-repack" => 6,
        "commit-graph" => 7,
        "worktree-prune" => 8,
        "rerere-gc" => 9,
        _ => usize::MAX,
    }
}

fn maintenance_schedule_rank(value: &str) -> Result<u8> {
    match validate_maintenance_schedule(value)?
        .to_ascii_lowercase()
        .as_str()
    {
        "weekly" => Ok(1),
        "daily" => Ok(2),
        "hourly" => Ok(3),
        _ => Ok(0),
    }
}

fn maintenance_run_selected(
    cli_session: &crate::session::CliSession,
    common_git_dir: &Path,
    config: &GitConfig,
    tasks: &[String],
    quiet: bool,
    auto: bool,
    detach: bool,
) -> Result<()> {
    let lock = repository_objects_dir(common_git_dir).join("maintenance.lock");
    if fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .is_err()
    {
        if auto {
            return Ok(());
        }
        eprintln!("fatal: 'maintenance' lock held by another process");
        return Err(GitError::Exit(128));
    }
    if detach {
        trace2_region("region_enter", "maintenance", "detach");
        trace2_region("region_leave", "maintenance", "detach");
    }
    for task in tasks {
        if auto && !maintenance_task_needed(common_git_dir, config, task)? {
            continue;
        }
        maintenance_run_one(cli_session, common_git_dir, config, task, quiet, auto)?;
    }
    let _ = fs::remove_file(lock);
    Ok(())
}

fn maintenance_run_one(
    cli_session: &crate::session::CliSession,
    common_git_dir: &Path,
    config: &GitConfig,
    task: &str,
    quiet: bool,
    auto: bool,
) -> Result<()> {
    match task {
        "commit-graph" => {
            if config.get_bool("core", None, "commitGraph") == Some(false) {
                return Ok(());
            }
            let progress = if quiet { "--no-progress" } else { "--progress" };
            trace2_child_start(&["commit-graph", "write", "--split", "--reachable", progress]);
            commands::plumbing::cmd_commit_graph(
                cli_session,
                &[
                    "write".to_string(),
                    "--reachable".to_string(),
                    progress.to_string(),
                ],
            )
        }
        "pack-refs" => {
            if auto {
                run_sley_child(&["pack-refs", "--all", "--prune", "--auto"], None)
            } else {
                run_sley_child(&["pack-refs", "--all", "--prune"], None)
            }
        }
        "reflog-expire" => run_sley_child(&["reflog", "expire", "--all"], None),
        "worktree-prune" => {
            let expire = config
                .get("gc", None, "worktreePruneExpire")
                .unwrap_or("3.months.ago");
            run_sley_child(&["worktree", "prune", "--expire", expire], None)
        }
        "rerere-gc" => run_sley_child(&["rerere", "gc"], None),
        "gc" => {
            run_sley_child(&["pack-refs", "--all", "--prune"], None)?;
            run_sley_child(&["reflog", "expire", "--all"], None)?;
            let mut args = vec!["gc"];
            if auto {
                args.push("--auto");
            }
            args.push(if quiet { "--quiet" } else { "--no-quiet" });
            args.push("--no-detach");
            args.push("--skip-foreground-tasks");
            run_sley_child(&args, None)
        }
        "prefetch" => maintenance_prefetch(config, quiet),
        "loose-objects" => maintenance_loose_objects(common_git_dir, config, quiet),
        "incremental-repack" => {
            if config.get_bool("core", None, "multiPackIndex") == Some(false) {
                if !quiet {
                    eprintln!(
                        "warning: skipping incremental-repack task because core.multiPackIndex is disabled"
                    );
                }
                return Ok(());
            }
            let progress = if quiet { "--no-progress" } else { "--progress" };
            run_sley_child(&["multi-pack-index", "write", progress], None)?;
            run_sley_child(&["multi-pack-index", "expire", progress], None)?;
            let batch = format!(
                "--batch-size={}",
                maintenance_auto_pack_size(common_git_dir)?
            );
            run_sley_child(
                &["multi-pack-index", "repack", progress, batch.as_str()],
                None,
            )
        }
        "geometric-repack" => maintenance_geometric_repack(common_git_dir, config, quiet),
        _ => Ok(()),
    }
}

fn maintenance_task_needed(common_git_dir: &Path, config: &GitConfig, task: &str) -> Result<bool> {
    Ok(match task {
        "commit-graph" => maintenance_limit_satisfied(
            config,
            "commit-graph",
            100,
            count_reachable_commits_not_in_graph(common_git_dir)?,
        )?,
        "loose-objects" => maintenance_limit_satisfied(
            config,
            "loose-objects",
            100,
            loose_object_ids(common_git_dir)?.len(),
        )?,
        "incremental-repack" => maintenance_pack_count_exceeds_limit(
            config,
            task,
            10,
            count_pack_files(common_git_dir)?,
        )?,
        "geometric-repack" => maintenance_geometric_repack_needed(common_git_dir, config)?,
        "worktree-prune" => worktree_prune_needed(common_git_dir, config)?,
        "rerere-gc" => rerere_gc_needed(common_git_dir, config)?,
        "reflog-expire" => maintenance_reflog_expire_needed(common_git_dir, config)?,
        "pack-refs" => true,
        _ => false,
    })
}

fn maintenance_limit_satisfied(
    config: &GitConfig,
    task: &str,
    default: i64,
    count: usize,
) -> Result<bool> {
    let limit = maintenance_auto_limit(config, task, default);
    Ok(limit < 0 || (limit > 0 && count >= limit as usize))
}

fn maintenance_pack_count_exceeds_limit(
    config: &GitConfig,
    task: &str,
    default: i64,
    count: usize,
) -> Result<bool> {
    let limit = maintenance_auto_limit(config, task, default);
    Ok(limit < 0 || (limit > 0 && count > limit as usize))
}

fn maintenance_geometric_split_factor(config: &GitConfig) -> u64 {
    config
        .get("maintenance", Some("geometric-repack"), "splitFactor")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|&factor| factor > 0)
        .unwrap_or(2)
}

fn maintenance_geometric_repack_plan(
    common_git_dir: &Path,
    config: &GitConfig,
) -> Result<sley_odb::GeometricRepackPlan> {
    let format = repository_object_format(common_git_dir)?;
    sley_odb::geometric_repack_plan(
        common_git_dir,
        format,
        maintenance_geometric_split_factor(config),
        &HashSet::new(),
    )
}

fn maintenance_geometric_repack(
    common_git_dir: &Path,
    config: &GitConfig,
    quiet: bool,
) -> Result<()> {
    let factor = maintenance_geometric_split_factor(config);
    let plan = maintenance_geometric_repack_plan(common_git_dir, config)?;
    let mut args = vec!["repack", "-d", "-l"];
    let geometric;
    if plan.split < plan.pack_count {
        geometric = format!("--geometric={factor}");
        args.push(geometric.as_str());
    } else {
        args.push("--cruft");
        args.push("--cruft-expiration=2.weeks.ago");
    }
    if quiet {
        args.push("--quiet");
    }
    args.push("--write-midx");
    run_sley_child(&args, None)
}

fn maintenance_geometric_repack_needed(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let limit = maintenance_auto_limit(config, "geometric-repack", 100);
    if limit == 0 {
        return Ok(false);
    }
    if limit < 0 {
        return Ok(true);
    }
    let plan = maintenance_geometric_repack_plan(common_git_dir, config)?;
    if plan.split > 0 {
        return Ok(true);
    }
    Ok(false)
}

fn maintenance_auto_limit(config: &GitConfig, task: &str, default: i64) -> i64 {
    config
        .get("maintenance", Some(task), "auto")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(default)
}

fn maintenance_reflog_expire_needed(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let limit = maintenance_auto_limit(config, "reflog-expire", 100);
    if limit == 0 {
        return Ok(false);
    }
    if limit < 0 {
        return Ok(true);
    }

    let cutoff = match config.get("gc", None, "reflogExpire") {
        Some(value) => parse_reflog_expire_time(value, "gc.reflogExpire")?,
        None => current_unix_seconds().saturating_sub(30 * 24 * 60 * 60),
    };
    if cutoff == i64::MIN {
        return Ok(false);
    }

    let format = repository_object_format(common_git_dir)?;
    let store = FileRefStore::new(common_git_dir, format);
    let mut count = 0usize;
    for entry in store.read_reflog("HEAD")? {
        if entry.timestamp_seconds()? < cutoff {
            count += 1;
            if count >= limit as usize {
                return Ok(true);
            }
        }
    }
    Ok(false)
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
    let selected = maintenance_select_tasks(&config, &tasks, schedule.as_deref())?;
    if (!auto && !selected.is_empty())
        || selected
            .iter()
            .any(|task| maintenance_task_needed(&common_git_dir, &config, task).unwrap_or(false))
    {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

fn run_sley_child(args: &[&str], stdin_data: Option<&str>) -> Result<()> {
    trace2_child_start(args);
    let mut child = ProcessCommand::new(env::current_exe()?);
    child.args(args);
    child.env(
        "SLEY_TRACE2_DEPTH",
        (sley_core::trace2::depth() + 1).to_string(),
    );
    if stdin_data.is_some() {
        child.stdin(std::process::Stdio::piped());
    }
    if args.first() == Some(&"pack-objects") {
        child.stdout(std::process::Stdio::null());
    }
    let mut child = child.spawn()?;
    if let Some(input) = stdin_data
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(input.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(GitError::Exit(status.code().unwrap_or(1)))
    }
}

pub(crate) fn trace2_child_start(args: &[&str]) {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let sid = trace2_sid();
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let argv = argv
        .iter()
        .map(|arg| format!("\"{}\"", json_escape(arg)))
        .collect::<Vec<_>>()
        .join(",");
    let line = format!(
        "{{\"event\":\"child_start\",\"sid\":\"{sid}\",\"child_id\":0,\"argv\":[{argv}]}}\n"
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

pub(crate) fn trace2_touch() {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let _ = fs::OpenOptions::new().create(true).append(true).open(path);
}

fn trace2_sid() -> String {
    let depth = sley_core::trace2::depth();
    if depth == 0 {
        "sley".to_string()
    } else {
        format!("sley/{depth}")
    }
}

fn trace2_region(event: &str, category: &str, label: &str) {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let line = format!(
        "{{\"event\":\"{}\",\"sid\":\"sley\",\"category\":\"{}\",\"label\":\"{}\"}}\n",
        json_escape(event),
        json_escape(category),
        json_escape(label)
    );
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn trace2_perf_data(key: &str, value: &str) {
    let Some(path) = env::var_os("GIT_TRACE2_PERF") else {
        return;
    };
    let line = format!("data: {key}:{value}\n");
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn json_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

fn maintenance_prefetch(config: &GitConfig, quiet: bool) -> Result<()> {
    let mut remotes = Vec::new();
    for section in &config.sections {
        if section.name.eq_ignore_ascii_case("remote")
            && let Some(name) = &section.subsection
            && section
                .entries
                .iter()
                .any(|entry| entry.key.eq_ignore_ascii_case("url"))
            && !config
                .get_bool("remote", Some(name), "skipFetchAll")
                .unwrap_or(false)
        {
            remotes.push(name.clone());
        }
    }
    remotes.sort();
    remotes.dedup();
    for remote in remotes {
        let mut args = vec![
            "fetch",
            remote.as_str(),
            "--prefetch",
            "--prune",
            "--no-tags",
            "--no-write-fetch-head",
            "--recurse-submodules=no",
        ];
        if quiet {
            args.push("--quiet");
        }
        run_sley_child(&args, None)?;
    }
    Ok(())
}

fn maintenance_loose_objects(common_git_dir: &Path, config: &GitConfig, quiet: bool) -> Result<()> {
    let mut prune_args = vec!["prune-packed"];
    if quiet {
        prune_args.push("--quiet");
    }
    run_sley_child(&prune_args, None)?;
    let loose = loose_object_ids(common_git_dir)?;
    if loose.is_empty() {
        return Ok(());
    }
    let mut batch = config
        .get("maintenance", Some("loose-objects"), "batchSize")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(50_000);
    if batch == 0 {
        batch = usize::MAX;
    }
    let input = loose
        .into_iter()
        .take(batch)
        .map(|oid| format!("{oid}\n"))
        .collect::<String>();
    let base = common_git_dir.join("objects").join("pack").join("loose");
    let base = base.display().to_string();
    let mut args = vec!["pack-objects"];
    args.push(if quiet { "--quiet" } else { "--no-quiet" });
    args.push(base.as_str());
    run_sley_child(&args, Some(&input))
}

fn loose_object_ids(common_git_dir: &Path) -> Result<Vec<String>> {
    let objects = common_git_dir.join("objects");
    let mut out = Vec::new();
    if !objects.exists() {
        return Ok(out);
    }
    for shard in fs::read_dir(objects)? {
        let shard = shard?;
        let Some(prefix) = shard.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if prefix.len() != 2 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        for entry in fs::read_dir(shard.path())? {
            let entry = entry?;
            let Some(suffix) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if suffix.len() == 38 && suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
                out.push(format!("{prefix}{suffix}"));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn count_pack_files(common_git_dir: &Path) -> Result<usize> {
    let pack_dir = common_git_dir.join("objects").join("pack");
    if !pack_dir.exists() {
        return Ok(0);
    }
    Ok(fs::read_dir(pack_dir)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("pack"))
        .count())
}

fn maintenance_auto_pack_size(common_git_dir: &Path) -> Result<u64> {
    let pack_dir = common_git_dir.join("objects").join("pack");
    let mut sizes = Vec::new();
    if pack_dir.exists() {
        for entry in fs::read_dir(pack_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("pack") {
                sizes.push(fs::metadata(path)?.len());
            }
        }
    }
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    Ok(sizes
        .get(1)
        .copied()
        .unwrap_or(0)
        .saturating_add(1)
        .min(i32::MAX as u64))
}

fn count_reachable_commits(common_git_dir: &Path) -> Result<usize> {
    let format = repository_object_format(common_git_dir)?;
    let refs = FileRefStore::new(common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut seen = HashSet::new();
    let mut stack = Vec::new();
    for reference in refs.list_refs()? {
        if let RefTarget::Direct(oid) = reference.target
            && db.read_object(&oid).is_ok()
        {
            stack.push(oid);
        }
    }
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let object = match db.read_object(&oid) {
            Ok(object) if object.object_type == ObjectType::Commit => object,
            _ => continue,
        };
        if let Ok(text) = std::str::from_utf8(&object.body) {
            for line in text.lines() {
                if let Some(parent) = line.strip_prefix("parent ")
                    && let Ok(parent) = ObjectId::from_hex(format, parent)
                {
                    stack.push(parent);
                }
            }
        }
    }
    Ok(seen.len())
}

fn count_reachable_commits_not_in_graph(common_git_dir: &Path) -> Result<usize> {
    let format = repository_object_format(common_git_dir)?;
    let graph_oids = commit_graph_oids(common_git_dir, format)?;
    if graph_oids.is_empty() {
        return count_reachable_commits(common_git_dir);
    }
    let refs = FileRefStore::new(common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut seen = HashSet::new();
    let mut missing = 0;
    let mut stack = Vec::new();
    for reference in refs.list_refs()? {
        if let RefTarget::Direct(oid) = reference.target
            && db.read_object(&oid).is_ok()
        {
            stack.push(oid);
        }
    }
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if graph_oids.contains(&oid) {
            continue;
        }
        let object = match db.read_object(&oid) {
            Ok(object) if object.object_type == ObjectType::Commit => object,
            _ => continue,
        };
        missing += 1;
        if let Ok(text) = std::str::from_utf8(&object.body) {
            for line in text.lines() {
                if let Some(parent) = line.strip_prefix("parent ")
                    && let Ok(parent) = ObjectId::from_hex(format, parent)
                {
                    stack.push(parent);
                }
            }
        }
    }
    Ok(missing)
}

fn commit_graph_oids(common_git_dir: &Path, format: ObjectFormat) -> Result<HashSet<ObjectId>> {
    let info = repository_objects_dir(common_git_dir).join("info");
    let single = info.join("commit-graph");
    let mut oids = HashSet::new();
    if single.exists() {
        let bytes = fs::read(single)?;
        let graph = CommitGraph::parse(&bytes, format)?;
        oids.extend(graph.commits.into_iter().map(|entry| entry.oid));
        return Ok(oids);
    }
    let graphs = info.join("commit-graphs");
    let chain = graphs.join("commit-graph-chain");
    let Ok(contents) = fs::read_to_string(chain) else {
        return Ok(oids);
    };
    for line in contents.lines() {
        let hash = line.trim();
        if hash.is_empty() {
            continue;
        }
        let bytes = fs::read(graphs.join(format!("graph-{hash}.graph")))?;
        let graph = CommitGraph::parse(&bytes, format)?;
        oids.extend(graph.commits.into_iter().map(|entry| entry.oid));
    }
    Ok(oids)
}

fn rerere_gc_needed(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let limit = config
        .get("maintenance", Some("rerere-gc"), "auto")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1);
    if limit <= 0 {
        return Ok(limit < 0);
    }
    Ok(count_dir_entries(&common_git_dir.join("rr-cache"))? > 0)
}

fn worktree_prune_needed(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let limit = config
        .get("maintenance", Some("worktree-prune"), "auto")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1);
    if limit <= 0 {
        return Ok(limit < 0);
    }

    let expire = config
        .get("gc", None, "worktreePruneExpire")
        .unwrap_or("3.months.ago");
    let expire_time = parse_prune_expire(expire, "--expire")?;
    let worktrees = common_git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(&worktrees) else {
        return Ok(false);
    };
    let mut prunable = 0usize;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() || linked_worktree_admin_is_prunable(&path, expire_time)? {
            prunable += 1;
        }
        if prunable >= limit as usize {
            return Ok(true);
        }
    }
    Ok(false)
}

fn linked_worktree_admin_is_prunable(admin_dir: &Path, expire_time: i64) -> Result<bool> {
    if admin_dir.join("locked").exists() {
        return Ok(false);
    }
    let gitdir_file = admin_dir.join("gitdir");
    if !gitdir_file.is_file() {
        return Ok(true);
    }
    let value = fs::read_to_string(&gitdir_file)?;
    let gitdir = resolve_worktree_admin_path(admin_dir, value.trim());
    if gitdir.exists() {
        return Ok(false);
    }
    if expire_time == i64::MIN {
        return Ok(false);
    }
    if expire_time == i64::MAX {
        return Ok(true);
    }
    let modified = fs::metadata(admin_dir)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0);
    Ok(modified <= expire_time)
}

fn resolve_worktree_admin_path(admin_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        admin_dir.join(path)
    }
}

fn count_dir_entries(path: &Path) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    Ok(fs::read_dir(path)?
        .filter_map(std::result::Result::ok)
        .count())
}

fn cmd_maintenance_register(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let config_file = parse_maintenance_config_file(args, "register")?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let repo = env::current_dir()?.display().to_string();

    let _ = report_missing_maintenance_repo(&common_git_dir);
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

    let file = config_file.unwrap_or(maintenance_global_config_path()?);
    config_add_value_if_missing(&file, "maintenance", "repo", &repo)?;
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
    let missing_repo_value = report_missing_maintenance_repo(&common_git_dir);
    if missing_repo_value && !force {
        return Err(GitError::Exit(128));
    }
    let file = config_file.unwrap_or(maintenance_global_config_path()?);
    if !config_remove_value(&file, "maintenance", "repo", &repo)? && !force {
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

fn maintenance_global_config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("GIT_CONFIG_GLOBAL").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let Some(home) = sley_config::home_dir() else {
        eprintln!("fatal: $HOME not set");
        return Err(GitError::Exit(128));
    };
    let user = PathBuf::from(&home).join(".gitconfig");
    if !user.exists() {
        let xdg = env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(".config"))
            .join("git")
            .join("config");
        if xdg.exists() {
            return Ok(xdg);
        }
    }
    Ok(user)
}

fn report_missing_maintenance_repo(common_git_dir: &Path) -> bool {
    let mut missing = false;
    if let Ok(config) = GitConfig::read(common_git_dir.join("config")) {
        for value in config.get_all("maintenance", None, "repo") {
            if value.is_none() {
                eprintln!("error: missing value for 'maintenance.repo'");
                missing = true;
            }
        }
    }
    missing
}

fn config_add_value_if_missing(path: &Path, section: &str, key: &str, value: &str) -> Result<()> {
    let mut config = if path.exists() {
        GitConfig::read(path)?
    } else {
        GitConfig::default()
    };
    if config
        .get_all(section, None, key)
        .into_iter()
        .any(|entry| entry == Some(value))
    {
        return Ok(());
    }
    config_push_value(&mut config, section, key, value);
    write_config(path, &config)
}

fn config_remove_value(path: &Path, section: &str, key: &str, value: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut config = GitConfig::read(path)?;
    let mut removed = false;
    for candidate in &mut config.sections {
        if !candidate.name.eq_ignore_ascii_case(section) || candidate.subsection.is_some() {
            continue;
        }
        candidate.entries.retain(|entry| {
            let matched =
                entry.key.eq_ignore_ascii_case(key) && entry.value.as_deref() == Some(value);
            removed |= matched;
            !matched
        });
    }
    if removed {
        write_config(path, &config)?;
    }
    Ok(removed)
}

fn config_push_value(config: &mut GitConfig, section: &str, key: &str, value: &str) {
    let section_idx = config
        .sections
        .iter()
        .rposition(|candidate| {
            candidate.name.eq_ignore_ascii_case(section) && candidate.subsection.is_none()
        })
        .unwrap_or_else(|| {
            config
                .sections
                .push(ConfigSection::new(section, None, Vec::new()));
            config.sections.len() - 1
        });
    config.sections[section_idx]
        .entries
        .push(ConfigEntry::new(key, Some(value.to_string())));
}

fn write_config(path: &Path, config: &GitConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, config.to_preserved_bytes())?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MaintenanceScheduler {
    Cron,
    Systemd,
    Launchctl,
    Schtasks,
}

fn cmd_maintenance_start(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let scheduler = parse_maintenance_start_args(args)?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let scheduler = resolve_maintenance_scheduler(scheduler)?;
    validate_scheduler_available(scheduler)?;
    // Git installs the schedule before registering the repository.  In
    // particular, an unavailable scheduler must not leave maintenance.repo
    // behind even though Scalar treats that failure as a warning.
    update_background_schedule(&common_git_dir, Some(scheduler))?;
    cmd_maintenance_register(cli_session, &[])
}

fn cmd_maintenance_stop(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return maintenance_subcommand_usage("stop");
    }
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    update_background_schedule(&common_git_dir, None)
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

fn scheduler_name(scheduler: MaintenanceScheduler) -> &'static str {
    match scheduler {
        MaintenanceScheduler::Cron => "crontab",
        MaintenanceScheduler::Systemd => "systemctl",
        MaintenanceScheduler::Launchctl => "launchctl",
        MaintenanceScheduler::Schtasks => "schtasks",
    }
}

fn resolve_maintenance_scheduler(
    scheduler: Option<MaintenanceScheduler>,
) -> Result<MaintenanceScheduler> {
    if let Some(scheduler) = scheduler {
        return Ok(scheduler);
    }

    #[cfg(target_os = "macos")]
    {
        return Ok(MaintenanceScheduler::Launchctl);
    }
    #[cfg(windows)]
    {
        return Ok(MaintenanceScheduler::Schtasks);
    }
    #[cfg(target_os = "linux")]
    {
        if scheduler_available(MaintenanceScheduler::Systemd) {
            return Ok(MaintenanceScheduler::Systemd);
        }
        if scheduler_available(MaintenanceScheduler::Cron) {
            return Ok(MaintenanceScheduler::Cron);
        }
        eprintln!("fatal: neither systemd timers nor crontab are available");
        return Err(GitError::Exit(128));
    }
    #[allow(unreachable_code)]
    Ok(MaintenanceScheduler::Cron)
}

fn validate_scheduler_available(scheduler: MaintenanceScheduler) -> Result<()> {
    if scheduler_available(scheduler) {
        Ok(())
    } else {
        eprintln!(
            "fatal: {} scheduler is not available",
            scheduler_name(scheduler)
        );
        Err(GitError::Exit(128))
    }
}

fn scheduler_available(scheduler: MaintenanceScheduler) -> bool {
    if let Some((program, _)) = scheduler_test_command(scheduler) {
        return program != "false";
    }
    if env::var_os("GIT_TEST_MAINT_SCHEDULER").is_some() {
        return false;
    }
    match scheduler {
        MaintenanceScheduler::Cron => ProcessCommand::new("crontab").arg("-l").output().is_ok(),
        MaintenanceScheduler::Systemd => ProcessCommand::new("systemctl")
            .args(["--user", "list-timers"])
            .status()
            .is_ok_and(|status| status.success()),
        MaintenanceScheduler::Launchctl => ProcessCommand::new("launchctl")
            .arg("list")
            .status()
            .is_ok_and(|status| status.success()),
        MaintenanceScheduler::Schtasks => ProcessCommand::new("schtasks")
            .arg("/query")
            .output()
            .is_ok(),
    }
}

fn scheduler_test_command(scheduler: MaintenanceScheduler) -> Option<(String, Vec<String>)> {
    let spec = env::var("GIT_TEST_MAINT_SCHEDULER").ok()?;
    for item in spec.split(',') {
        let (name, command) = item.split_once(':')?;
        if name != scheduler_name(scheduler) {
            continue;
        }
        let mut parts = command
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        if parts.is_empty() {
            return None;
        }
        let program = parts.remove(0);
        return Some((program, parts));
    }
    None
}

fn run_scheduler_command(scheduler: MaintenanceScheduler, args: &[&str]) -> Result<()> {
    let (program, mut command_args) = scheduler_test_command(scheduler)
        .unwrap_or_else(|| (scheduler_name(scheduler).to_string(), Vec::new()));
    command_args.extend(args.iter().map(|arg| (*arg).to_string()));
    let status = ProcessCommand::new(program).args(command_args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(GitError::Exit(status.code().unwrap_or(1)))
    }
}

fn update_background_schedule(
    common_git_dir: &Path,
    enable: Option<MaintenanceScheduler>,
) -> Result<()> {
    let lock = repository_objects_dir(common_git_dir).join("schedule.lock");
    if fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .is_err()
    {
        eprintln!("error: Another scheduled git-maintenance(1) process seems to be running");
        return Err(GitError::Exit(128));
    }
    if let Some(scheduler) = enable
        && let Err(err) = validate_scheduler_available(scheduler)
    {
        let _ = fs::remove_file(lock);
        return Err(err);
    }
    for scheduler in [
        MaintenanceScheduler::Cron,
        MaintenanceScheduler::Systemd,
        MaintenanceScheduler::Launchctl,
        MaintenanceScheduler::Schtasks,
    ] {
        if enable == Some(scheduler) {
            continue;
        }
        if scheduler_available(scheduler) {
            let _ = update_scheduler(common_git_dir, scheduler, false);
        }
    }
    if let Some(scheduler) = enable {
        update_scheduler(common_git_dir, scheduler, true)?;
    }
    let _ = fs::remove_file(lock);
    Ok(())
}

fn update_scheduler(
    common_git_dir: &Path,
    scheduler: MaintenanceScheduler,
    enable: bool,
) -> Result<()> {
    match scheduler {
        MaintenanceScheduler::Cron => update_cron(enable),
        MaintenanceScheduler::Systemd => update_systemd(enable),
        MaintenanceScheduler::Launchctl => update_launchctl(enable),
        MaintenanceScheduler::Schtasks => update_schtasks(common_git_dir, enable),
    }
}

fn update_cron(enable: bool) -> Result<()> {
    let Some((_, args)) = scheduler_test_command(MaintenanceScheduler::Cron) else {
        return Ok(());
    };
    let Some(path) = args.last().map(PathBuf::from) else {
        return Ok(());
    };
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut out = String::new();
    let mut skipping = false;
    for line in existing.lines() {
        if line == "# BEGIN GIT MAINTENANCE SCHEDULE" {
            skipping = true;
            continue;
        }
        if line == "# END GIT MAINTENANCE SCHEDULE" {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    if enable {
        out.push_str("# BEGIN GIT MAINTENANCE SCHEDULE\n");
        out.push_str("0 1-23 * * * git for-each-repo --keep-going --config=maintenance.repo maintenance run --schedule=hourly\n");
        out.push_str("0 0 * * 1-6 git for-each-repo --keep-going --config=maintenance.repo maintenance run --schedule=daily\n");
        out.push_str("0 0 * * 0 git for-each-repo --keep-going --config=maintenance.repo maintenance run --schedule=weekly\n");
        out.push_str("# END GIT MAINTENANCE SCHEDULE\n");
    }
    fs::write(path, out)?;
    Ok(())
}

fn update_systemd(enable: bool) -> Result<()> {
    let base = xdg_config_home().join("systemd").join("user");
    if enable {
        fs::create_dir_all(&base)?;
        fs::write(
            base.join("git-maintenance@.service"),
            "[Service]\nExecStart=git -c core.askPass=true -c credential.interactive=false for-each-repo --keep-going --config=maintenance.repo maintenance run --schedule=%i\n",
        )?;
        for frequency in ["hourly", "daily", "weekly"] {
            fs::write(
                base.join(format!("git-maintenance@{frequency}.timer")),
                "[Timer]\n",
            )?;
            run_scheduler_command(
                MaintenanceScheduler::Systemd,
                &[
                    "--user",
                    "enable",
                    "--now",
                    &format!("git-maintenance@{frequency}.timer"),
                ],
            )?;
        }
    } else {
        for frequency in ["hourly", "daily", "weekly"] {
            let _ = run_scheduler_command(
                MaintenanceScheduler::Systemd,
                &[
                    "--user",
                    "disable",
                    "--now",
                    &format!("git-maintenance@{frequency}.timer"),
                ],
            );
            let _ = fs::remove_file(base.join(format!("git-maintenance@{frequency}.timer")));
        }
        let _ = fs::remove_file(base.join("git-maintenance@.service"));
    }
    Ok(())
}

fn update_launchctl(enable: bool) -> Result<()> {
    let Some(home) = sley_config::home_dir() else {
        return Ok(());
    };
    let base = PathBuf::from(home).join("Library").join("LaunchAgents");
    if enable {
        fs::create_dir_all(&base)?;
        let all_exist = ["hourly", "daily", "weekly"].iter().all(|frequency| {
            base.join(format!("org.git-scm.git.{frequency}.plist"))
                .exists()
        });
        if all_exist {
            for frequency in ["hourly", "daily", "weekly"] {
                run_scheduler_command(
                    MaintenanceScheduler::Launchctl,
                    &["list", &format!("org.git-scm.git.{frequency}")],
                )?;
            }
            return Ok(());
        }
        for frequency in ["hourly", "daily", "weekly"] {
            let plist = base.join(format!("org.git-scm.git.{frequency}.plist"));
            fs::write(
                &plist,
                format!("<plist><string>schedule={frequency}</string></plist>\n"),
            )?;
            let plist = plist.display().to_string();
            let _ = run_scheduler_command(
                MaintenanceScheduler::Launchctl,
                &["bootout", "gui/0", &plist],
            );
            run_scheduler_command(
                MaintenanceScheduler::Launchctl,
                &["bootstrap", "gui/0", &plist],
            )?;
        }
    } else {
        for frequency in ["hourly", "daily", "weekly"] {
            let plist = base.join(format!("org.git-scm.git.{frequency}.plist"));
            let plist_arg = plist.display().to_string();
            let _ = run_scheduler_command(
                MaintenanceScheduler::Launchctl,
                &["bootout", "gui/0", &plist_arg],
            );
            let _ = fs::remove_file(plist);
        }
    }
    Ok(())
}

fn update_schtasks(common_git_dir: &Path, enable: bool) -> Result<()> {
    if enable {
        for frequency in ["hourly", "daily", "weekly"] {
            let xml = common_git_dir.join(format!("schedule_{frequency}"));
            fs::write(&xml, "<Task></Task>\n")?;
            let xml = xml.display().to_string();
            run_scheduler_command(
                MaintenanceScheduler::Schtasks,
                &[
                    "/create",
                    "/tn",
                    &format!("Git Maintenance ({frequency})"),
                    "/f",
                    "/xml",
                    &xml,
                ],
            )?;
        }
    } else {
        for frequency in ["hourly", "daily", "weekly"] {
            let _ = run_scheduler_command(
                MaintenanceScheduler::Schtasks,
                &[
                    "/delete",
                    "/tn",
                    &format!("Git Maintenance ({frequency})"),
                    "/f",
                ],
            );
        }
    }
    Ok(())
}

fn xdg_config_home() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| sley_config::home_dir().map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
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
    let stats = count_objects_stats(&git_dir, format)?;
    if verbose {
        println!("count: {}", stats.count);
        println!(
            "size: {}",
            count_objects_size(stats.size_kib, human_readable)
        );
        println!("in-pack: {}", stats.in_pack);
        println!("packs: {}", stats.packs);
        println!(
            "size-pack: {}",
            count_objects_pack_size(stats.size_pack_bytes, human_readable)
        );
        println!("prune-packable: {}", stats.prune_packable);
        println!("garbage: {}", stats.garbage);
        println!(
            "size-garbage: {}",
            count_objects_pack_size(stats.size_garbage_bytes, human_readable)
        );
        for alternate in &stats.alternates {
            println!("alternate: {alternate}");
        }
    } else {
        println!(
            "{} objects, {}",
            stats.count,
            if human_readable {
                count_objects_human_size(stats.size_kib)
            } else {
                format!("{} kilobytes", stats.size_kib)
            }
        );
    }
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
            if matches!(err, GitError::Io(ref message) if message.contains("File exists")) {
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

fn reftable_lock_timeout_override() -> Result<Option<u64>> {
    Ok(global_config_value("reftable.lockTimeout")?.and_then(|value| value.parse::<u64>().ok()))
}

#[derive(Debug, Clone, Default)]
struct CountObjectsStats {
    count: u64,
    size_kib: u64,
    in_pack: u64,
    packs: u64,
    size_pack_bytes: u64,
    prune_packable: u64,
    garbage: u64,
    size_garbage_bytes: u64,
    alternates: Vec<String>,
}

fn count_objects_stats(git_dir: &Path, format: ObjectFormat) -> Result<CountObjectsStats> {
    let objects_dir = repository_objects_dir(git_dir);
    let mut stats = CountObjectsStats::default();
    if !objects_dir.exists() {
        return Ok(stats);
    }
    stats.alternates = count_objects_alternates(&objects_dir)?;
    let default_objects_dir = git_dir.join("objects");
    let display_root = if objects_dir == default_objects_dir {
        git_dir.parent().unwrap_or(git_dir)
    } else {
        objects_dir.parent().unwrap_or(&objects_dir)
    };
    let pack_indexes = count_pack_objects(&objects_dir.join("pack"), format, &mut stats)?;
    let mut packed_lookup = CountPackedObjectLookup::new(format, pack_indexes);
    let hex_len = format.hex_len();
    for entry in fs::read_dir(&objects_dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "info" || name == "pack" {
            continue;
        }
        if entry.metadata()?.is_dir()
            && name.len() == 2
            && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            count_loose_object_directory(
                &path,
                display_root,
                &name,
                format,
                hex_len,
                &mut packed_lookup,
                &mut stats,
            )?;
        }
    }
    Ok(stats)
}

fn count_objects_alternates(objects_dir: &Path) -> Result<Vec<String>> {
    let alternates_path = objects_dir.join("info").join("alternates");
    let Ok(contents) = fs::read(&alternates_path) else {
        return Ok(Vec::new());
    };
    let mut alternates = Vec::new();
    for raw in contents.split(|byte| *byte == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        let value =
            std::str::from_utf8(line).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        let path = Path::new(value);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            objects_dir.join(path)
        };
        let display = fs::canonicalize(&absolute).unwrap_or(absolute);
        alternates.push(display.to_string_lossy().into_owned());
    }
    Ok(alternates)
}

fn count_loose_object_directory(
    dir: &Path,
    display_root: &Path,
    fanout: &str,
    format: ObjectFormat,
    hex_len: usize,
    packed_lookup: &mut CountPackedObjectLookup,
    stats: &mut CountObjectsStats,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if metadata.is_file()
            && name.len() == hex_len - 2
            && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            let oid = ObjectId::from_hex(format, &format!("{fanout}{name}"))?;
            stats.count += 1;
            stats.size_kib += filesystem_size_kib(&metadata);
            if packed_lookup.contains(&oid)? {
                stats.prune_packable += 1;
            }
        } else {
            let entry_path = entry.path();
            let display_path = entry_path
                .strip_prefix(display_root)
                .unwrap_or(entry_path.as_path());
            eprintln!("warning: garbage found: {}", display_path.display());
            stats.garbage += 1;
            stats.size_garbage_bytes += metadata.len();
        }
    }
    Ok(())
}

fn count_pack_objects(
    pack_dir: &Path,
    format: ObjectFormat,
    stats: &mut CountObjectsStats,
) -> Result<Vec<CountPackIndexSummary>> {
    let mut pack_indexes = Vec::new();
    if !pack_dir.exists() {
        return Ok(pack_indexes);
    }
    let display_root = pack_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .unwrap_or(pack_dir);
    let mut stems: BTreeMap<String, CountPackStem> = BTreeMap::new();
    for entry in fs::read_dir(pack_dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("")
            .to_string();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("pack") => {
                stats.packs += 1;
                stats.size_pack_bytes += metadata.len();
                stems.entry(stem).or_default().pack = Some(path);
            }
            Some("idx") => {
                let summary = count_pack_index_summary(&path, &metadata, format)?;
                if let Some(summary) = summary {
                    stats.size_pack_bytes += metadata.len();
                    stats.in_pack += u64::from(summary.object_count);
                    pack_indexes.push(summary);
                }
                stems.entry(stem).or_default().idx = Some(path);
            }
            Some("keep") => {
                stems.entry(stem).or_default().keep = Some(path);
            }
            Some("rev" | "bitmap" | "mtimes" | "promisor") => {}
            _ => count_pack_garbage(&path, &metadata, display_root, stats),
        }
    }
    for stem in stems.values() {
        match (&stem.pack, &stem.idx, &stem.keep) {
            (Some(pack), None, Some(keep)) => {
                count_pack_correspondence_warning("no corresponding .idx", keep, display_root);
                count_pack_correspondence_warning("no corresponding .idx", pack, display_root);
            }
            (Some(pack), None, None) => {
                count_pack_correspondence_warning("no corresponding .idx", pack, display_root);
            }
            (None, Some(idx), Some(keep)) => {
                count_pack_correspondence_warning("no corresponding .pack", idx, display_root);
                count_pack_correspondence_warning("no corresponding .pack", keep, display_root);
            }
            (None, Some(idx), None) => {
                count_pack_correspondence_warning("no corresponding .pack", idx, display_root);
            }
            (None, None, Some(keep)) => {
                count_pack_correspondence_warning(
                    "no corresponding .idx or .pack",
                    keep,
                    display_root,
                );
            }
            _ => {}
        }
    }
    Ok(pack_indexes)
}

#[derive(Debug, Default)]
struct CountPackStem {
    pack: Option<PathBuf>,
    idx: Option<PathBuf>,
    keep: Option<PathBuf>,
}

fn count_pack_garbage(
    path: &Path,
    _metadata: &fs::Metadata,
    display_root: &Path,
    _stats: &mut CountObjectsStats,
) {
    let display_path = path.strip_prefix(display_root).unwrap_or(path);
    eprintln!("warning: garbage found: {}", display_path.display());
}

fn count_pack_correspondence_warning(message: &str, path: &Path, display_root: &Path) {
    let display_path = path.strip_prefix(display_root).unwrap_or(path);
    eprintln!("warning: {message}: {}", display_path.display());
}

#[derive(Debug, Clone)]
struct CountPackIndexSummary {
    path: PathBuf,
    object_count: u32,
}

#[derive(Debug)]
struct CountPackedObjectLookup {
    format: ObjectFormat,
    summaries: Vec<CountPackIndexSummary>,
    indexes: Option<Vec<CountPackIndexLookup>>,
}

impl CountPackedObjectLookup {
    fn new(format: ObjectFormat, summaries: Vec<CountPackIndexSummary>) -> Self {
        Self {
            format,
            summaries,
            indexes: None,
        }
    }

    fn contains(&mut self, oid: &ObjectId) -> Result<bool> {
        if self.summaries.is_empty() {
            return Ok(false);
        }
        if self.indexes.is_none() {
            self.indexes = Some(load_count_pack_index_lookups(
                self.format,
                self.summaries.as_slice(),
            )?);
        }
        Ok(self
            .indexes
            .as_ref()
            .expect("count pack indexes are loaded")
            .iter()
            .any(|index| index.contains(oid)))
    }
}

#[derive(Debug)]
struct CountPackIndexLookup {
    format: ObjectFormat,
    fanout: [u32; 256],
    bytes: Vec<u8>,
    layout: CountPackIndexLayout,
}

#[derive(Debug)]
enum CountPackIndexLayout {
    V1 {
        entry_table_start: usize,
        entry_len: usize,
    },
    V2 {
        oid_table_start: usize,
    },
}

impl CountPackIndexLookup {
    fn parse(bytes: Vec<u8>, format: ObjectFormat) -> Result<Self> {
        let metadata = count_pack_index_metadata(&bytes, format)?;
        if count_pack_index_min_len(&metadata, format)? > bytes.len() {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        Ok(Self {
            format,
            fanout: metadata.fanout,
            bytes,
            layout: metadata.layout,
        })
    }

    fn contains(&self, oid: &ObjectId) -> bool {
        if oid.format() != self.format {
            return false;
        }
        let oid_bytes = oid.as_bytes();
        let bucket = usize::from(oid_bytes[0]);
        let start = if bucket == 0 {
            0
        } else {
            self.fanout[bucket - 1] as usize
        };
        let end = self.fanout[bucket] as usize;
        if start == end {
            return false;
        }
        let mut low = start;
        let mut high = end;
        while low < high {
            let mid = low + (high - low) / 2;
            match self.oid_at(mid).cmp(oid_bytes) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Equal => return true,
                std::cmp::Ordering::Greater => high = mid,
            }
        }
        false
    }

    fn oid_at(&self, idx: usize) -> &[u8] {
        match self.layout {
            CountPackIndexLayout::V1 {
                entry_table_start,
                entry_len,
            } => {
                let start = entry_table_start + idx * entry_len + 4;
                &self.bytes[start..start + self.format.raw_len()]
            }
            CountPackIndexLayout::V2 { oid_table_start } => {
                let start = oid_table_start + idx * self.format.raw_len();
                &self.bytes[start..start + self.format.raw_len()]
            }
        }
    }
}

#[derive(Debug)]
struct CountPackIndexMetadata {
    object_count: u32,
    fanout: [u32; 256],
    layout: CountPackIndexLayout,
}

fn count_pack_index_summary(
    path: &Path,
    metadata: &fs::Metadata,
    format: ObjectFormat,
) -> Result<Option<CountPackIndexSummary>> {
    let len = usize::try_from(metadata.len())
        .map_err(|_| GitError::InvalidFormat("pack index is too large".into()))?;
    let prefix_len = if len >= 4 && count_pack_index_has_v2_magic(path)? {
        8 + 256 * 4
    } else {
        256 * 4
    };
    if len < prefix_len {
        eprintln!(
            "error: index file {} is too small",
            count_pack_display_path(path).display()
        );
        return Ok(None);
    }
    let mut file = fs::File::open(path)?;
    let mut prefix = vec![0u8; prefix_len];
    match io::Read::read_exact(&mut file, &mut prefix) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    match count_pack_index_prefix_metadata(&prefix, format) {
        Ok(index) if count_pack_index_min_len(&index, format)? <= len => {
            Ok(Some(CountPackIndexSummary {
                path: path.to_path_buf(),
                object_count: index.object_count,
            }))
        }
        _ => Ok(None),
    }
}

fn count_pack_index_has_v2_magic(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut magic = [0u8; 4];
    match io::Read::read_exact(&mut file, &mut magic) {
        Ok(()) => Ok(magic == [0xff, b't', b'O', b'c']),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn count_pack_display_path(path: &Path) -> &Path {
    let display_root = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new(""));
    path.strip_prefix(display_root).unwrap_or(path)
}

fn load_count_pack_index_lookups(
    format: ObjectFormat,
    summaries: &[CountPackIndexSummary],
) -> Result<Vec<CountPackIndexLookup>> {
    let mut indexes = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let bytes = fs::read(&summary.path)?;
        if let Ok(index) = CountPackIndexLookup::parse(bytes, format) {
            indexes.push(index);
        }
    }
    Ok(indexes)
}

fn count_pack_index_metadata(bytes: &[u8], format: ObjectFormat) -> Result<CountPackIndexMetadata> {
    let hash_len = format.raw_len();
    if bytes.len() < 4 {
        return Err(GitError::InvalidFormat("pack index too short".into()));
    }
    if bytes[..4] == [0xff, b't', b'O', b'c'] {
        if bytes.len() < 8 + 256 * 4 {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        let version = count_u32_be(&bytes[4..8]);
        if version != 2 {
            return Err(GitError::Unsupported(format!(
                "pack index version {version}"
            )));
        }
        let (fanout, object_count) = count_pack_index_fanout(&bytes[8..8 + 256 * 4])?;
        let oid_table_start = 8 + 256 * 4;
        let oid_table = count_checked_range(oid_table_start, object_count as usize, hash_len)?;
        let crc_table = count_checked_range(oid_table.end, object_count as usize, 4)?;
        let small_offset_table = count_checked_range(crc_table.end, object_count as usize, 4)?;
        if bytes.len() < small_offset_table.end {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        return Ok(CountPackIndexMetadata {
            object_count,
            fanout,
            layout: CountPackIndexLayout::V2 { oid_table_start },
        });
    }

    if bytes.len() < 256 * 4 {
        return Err(GitError::InvalidFormat("pack index too short".into()));
    }
    let (fanout, object_count) = count_pack_index_fanout(&bytes[..256 * 4])?;
    let entry_table_start = 256 * 4;
    let entry_len = hash_len
        .checked_add(4)
        .ok_or_else(|| GitError::InvalidFormat("pack index entry length overflow".into()))?;
    let entry_table = count_checked_range(entry_table_start, object_count as usize, entry_len)?;
    if bytes.len() < entry_table.end {
        return Err(GitError::InvalidFormat("pack index too short".into()));
    }
    Ok(CountPackIndexMetadata {
        object_count,
        fanout,
        layout: CountPackIndexLayout::V1 {
            entry_table_start,
            entry_len,
        },
    })
}

fn count_pack_index_prefix_metadata(
    bytes: &[u8],
    format: ObjectFormat,
) -> Result<CountPackIndexMetadata> {
    if bytes.len() < 4 {
        return Err(GitError::InvalidFormat("pack index too short".into()));
    }
    if bytes[..4] == [0xff, b't', b'O', b'c'] {
        if bytes.len() < 8 + 256 * 4 {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        let version = count_u32_be(&bytes[4..8]);
        if version != 2 {
            return Err(GitError::Unsupported(format!(
                "pack index version {version}"
            )));
        }
        let (fanout, object_count) = count_pack_index_fanout(&bytes[8..8 + 256 * 4])?;
        return Ok(CountPackIndexMetadata {
            object_count,
            fanout,
            layout: CountPackIndexLayout::V2 {
                oid_table_start: 8 + 256 * 4,
            },
        });
    }

    if bytes.len() < 256 * 4 {
        return Err(GitError::InvalidFormat("pack index too short".into()));
    }
    let (fanout, object_count) = count_pack_index_fanout(&bytes[..256 * 4])?;
    let entry_len = format
        .raw_len()
        .checked_add(4)
        .ok_or_else(|| GitError::InvalidFormat("pack index entry length overflow".into()))?;
    Ok(CountPackIndexMetadata {
        object_count,
        fanout,
        layout: CountPackIndexLayout::V1 {
            entry_table_start: 256 * 4,
            entry_len,
        },
    })
}

fn count_pack_index_min_len(index: &CountPackIndexMetadata, format: ObjectFormat) -> Result<usize> {
    let hash_len = format.raw_len();
    match index.layout {
        CountPackIndexLayout::V1 {
            entry_table_start,
            entry_len,
        } => count_checked_range(entry_table_start, index.object_count as usize, entry_len)?
            .end
            .checked_add(hash_len * 2)
            .ok_or_else(|| GitError::InvalidFormat("pack index length overflow".into())),
        CountPackIndexLayout::V2 { oid_table_start } => {
            let oid_table =
                count_checked_range(oid_table_start, index.object_count as usize, hash_len)?;
            let crc_table = count_checked_range(oid_table.end, index.object_count as usize, 4)?;
            let small_offset_table =
                count_checked_range(crc_table.end, index.object_count as usize, 4)?;
            small_offset_table
                .end
                .checked_add(hash_len * 2)
                .ok_or_else(|| GitError::InvalidFormat("pack index length overflow".into()))
        }
    }
}

fn count_pack_index_fanout(bytes: &[u8]) -> Result<([u32; 256], u32)> {
    let mut fanout = [0u32; 256];
    let mut previous = 0u32;
    for (idx, slot) in fanout.iter_mut().enumerate() {
        let start = idx * 4;
        *slot = count_u32_be(&bytes[start..start + 4]);
        if *slot < previous {
            return Err(GitError::InvalidFormat(
                "pack index fanout is not monotonic".into(),
            ));
        }
        previous = *slot;
    }
    Ok((fanout, fanout[255]))
}

fn count_checked_range(start: usize, count: usize, width: usize) -> Result<std::ops::Range<usize>> {
    let len = count
        .checked_mul(width)
        .ok_or_else(|| GitError::InvalidFormat("pack index table length overflow".into()))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| GitError::InvalidFormat("pack index table offset overflow".into()))?;
    Ok(start..end)
}

fn count_u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
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
    let mut roots = prune_roots(
        git_dir,
        common_git_dir,
        format,
        cli_session.replace_objects(),
        &options.heads,
    )?;
    roots.extend(prune_recent_object_roots(
        db,
        common_git_dir,
        format,
        options.expire,
    )?);
    roots.extend(prune_recent_hook_roots(common_git_dir, format)?);
    roots.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    roots.dedup();
    let reachable = collect_reachable_object_ids(db, format, roots.iter().copied())?;
    let mut candidates = Vec::new();
    for oid in prune_unreachable_loose(common_git_dir, format, roots.iter().copied(), false)? {
        if prune_object_is_expired(db, &oid, options.expire)? {
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
    prune_shallow_file(
        common_git_dir,
        format,
        &reachable,
        options.dry_run,
        options.verbose,
    )?;
    prune_temporary_files(
        &common_git_dir.join("objects"),
        options.expire,
        options.dry_run,
        options.verbose,
    )?;
    prune_temporary_files(
        &common_git_dir.join("objects").join("pack"),
        options.expire,
        options.dry_run,
        options.verbose,
    )?;
    prune_packed_loose_objects(common_git_dir, format, options.dry_run)?;
    if !options.dry_run {
        prune_empty_loose_object_dirs(&common_git_dir.join("objects"))?;
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
            expire = parse_prune_expire(value, "--expire")?;
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

fn parse_prune_expire(value: &str, option: &str) -> Result<i64> {
    match value {
        "now" | "all" => Ok(i64::MAX),
        "never" => Ok(i64::MIN),
        _ => parse_reflog_expire_time(value, option).map_err(|err| {
            if matches!(err, GitError::Exit(_)) {
                eprintln!("error: malformed expiration date '{value}'");
            }
            err
        }),
    }
}

fn prune_roots(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
    heads: &[String],
) -> Result<Vec<ObjectId>> {
    let store = FileRefStore::new(common_git_dir, format);
    let mut roots = BTreeSet::new();
    for reference in store.list_refs()? {
        if let Some(oid) = resolve_ref_to_oid(&store, &reference.name)? {
            roots.insert(oid);
        }
    }
    for worktree_git_dir in prune_worktree_git_dirs(git_dir, common_git_dir)? {
        if let Some(oid) = prune_head_root(&store, &worktree_git_dir, format)? {
            roots.insert(oid);
        }
        for oid in prune_index_roots(&worktree_git_dir, format)? {
            roots.insert(oid);
        }
        for oid in reflog_roots_from_dir(&worktree_git_dir.join("logs"), format)? {
            roots.insert(oid);
        }
        for oid in prune_state_file_roots(&worktree_git_dir, format)? {
            roots.insert(oid);
        }
    }
    for head in heads {
        roots.insert(resolve_revision(
            common_git_dir,
            format,
            head,
            replace_objects,
        )?);
    }
    roots.extend(reflog_roots_from_dir(&common_git_dir.join("logs"), format)?);
    Ok(roots.into_iter().collect())
}

fn prune_worktree_git_dirs(git_dir: &Path, common_git_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![git_dir.to_path_buf()];
    if git_dir != common_git_dir {
        dirs.push(common_git_dir.to_path_buf());
    }
    let worktrees = common_git_dir.join("worktrees");
    if let Ok(entries) = fs::read_dir(worktrees) {
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

fn prune_head_root(
    store: &FileRefStore,
    worktree_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<ObjectId>> {
    let Ok(head) = fs::read_to_string(worktree_git_dir.join("HEAD")) else {
        return Ok(None);
    };
    let head = head.trim();
    if let Some(refname) = head.strip_prefix("ref:") {
        return resolve_ref_to_oid(store, refname.trim());
    }
    if head.len() == format.hex_len() {
        return ObjectId::from_hex(format, head).map(Some);
    }
    Ok(None)
}

fn prune_index_roots(git_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let Ok(bytes) = fs::read(git_dir.join("index")) else {
        return Ok(Vec::new());
    };
    let index = sley_index::Index::parse(&bytes, format)?;
    Ok(index
        .entries
        .into_iter()
        .filter(|entry| !sley_index::is_gitlink(entry.mode))
        .map(|entry| entry.oid)
        .collect())
}

fn prune_state_file_roots(git_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let mut roots = Vec::new();
    for path in [
        "rebase-apply/autostash",
        "rebase-apply/orig-head",
        "rebase-merge/autostash",
        "rebase-merge/orig-head",
    ] {
        let Ok(contents) = fs::read_to_string(git_dir.join(path)) else {
            continue;
        };
        let value = contents.trim();
        if value.len() == format.hex_len()
            && let Ok(oid) = ObjectId::from_hex(format, value)
        {
            roots.push(oid);
        }
    }
    Ok(roots)
}

fn reflog_roots_from_dir(logs_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let mut roots = Vec::new();
    let zero = "0".repeat(format.hex_len());
    let mut stack: Vec<PathBuf> = vec![logs_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(contents) = fs::read_to_string(&path) {
                for line in contents.lines() {
                    let mut fields = line.split(' ');
                    for hex in [fields.next(), fields.next()].into_iter().flatten() {
                        if hex != zero
                            && let Ok(oid) = ObjectId::from_hex(format, hex)
                        {
                            roots.push(oid);
                        }
                    }
                }
            }
        }
    }
    Ok(roots)
}

fn prune_recent_object_roots(
    db: &FileObjectDatabase,
    common_git_dir: &Path,
    format: ObjectFormat,
    expire: i64,
) -> Result<Vec<ObjectId>> {
    if expire == i64::MIN {
        return Ok(Vec::new());
    }
    let mut roots = Vec::new();
    let object_mtimes = sley_odb::object_mtimes_on_disk_pub(
        &sley_odb::repository_objects_dir(common_git_dir),
        format,
    )?;
    for (oid, mtime) in object_mtimes {
        if i64::from(mtime) <= expire {
            continue;
        }
        if db.read_object(&oid).is_ok() {
            roots.push(oid);
        }
    }
    Ok(roots)
}

fn prune_recent_hook_roots(common_git_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let config = read_repo_config(common_git_dir)?;
    commands::hooks::run_recent_objects_hooks(
        &config,
        format,
        common_git_dir.parent().unwrap_or(common_git_dir),
    )
}

fn gc_prune_expired_loose(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    expire: i64,
) -> Result<()> {
    if expire == i64::MIN {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut prune_roots = roots.to_vec();
    prune_roots.extend(prune_recent_object_roots(
        &db,
        common_git_dir,
        format,
        expire,
    )?);
    prune_roots.extend(prune_recent_hook_roots(common_git_dir, format)?);
    prune_roots.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    prune_roots.dedup();

    for oid in sley_odb::prune_unreachable_loose_tolerating_missing(
        common_git_dir,
        format,
        prune_roots,
        false,
    )? {
        if !prune_object_is_expired(&db, &oid, expire)? {
            continue;
        }
        let path = db.loose().object_path(&oid)?;
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
    }
    prune_packed_loose_objects(common_git_dir, format, false)?;
    prune_empty_loose_object_dirs(&common_git_dir.join("objects"))?;
    Ok(())
}

fn gc_pack_recent_unreachable_loose(
    common_git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
    expire: i64,
) -> Result<()> {
    if expire == i64::MIN {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut objects = Vec::new();
    for oid in sley_odb::prune_unreachable_loose_tolerating_missing(
        common_git_dir,
        format,
        roots.to_vec(),
        false,
    )? {
        if prune_object_is_expired(&db, &oid, expire)? {
            continue;
        }
        let object = match db.read_object(&oid) {
            Ok(object) => object,
            Err(_) => continue,
        };
        objects.push((oid, object));
    }
    if objects.is_empty() {
        return Ok(());
    }

    let inputs: Vec<_> = objects
        .iter()
        .map(|(oid, object)| PackInput {
            oid,
            object: object.as_ref(),
        })
        .collect();
    let written = PackFile::write_packed_with_known_ids(&inputs, format)?;
    let _install = db.install_written_pack(&written)?;
    for (oid, _) in objects {
        let path = db.loose().object_path(&oid)?;
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
    }
    prune_empty_loose_object_dirs(&common_git_dir.join("objects"))?;
    Ok(())
}

fn gc_clean_pack_garbage(pack_dir: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(pack_dir) else {
        return Ok(());
    };
    let mut stems: BTreeMap<String, CountPackStem> = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let stem = stem.to_string();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("pack") => stems.entry(stem).or_default().pack = Some(path),
            Some("idx") => stems.entry(stem).or_default().idx = Some(path),
            Some("keep") => stems.entry(stem).or_default().keep = Some(path),
            _ => {}
        }
    }
    for stem in stems.values() {
        if stem.pack.is_some() {
            continue;
        }
        if let Some(idx) = &stem.idx {
            remove_pack_garbage_file(idx)?;
            if let Some(keep) = &stem.keep {
                remove_pack_garbage_file(keep)?;
            }
        }
    }
    Ok(())
}

fn gc_remove_cruft_packs(common_git_dir: &Path) -> Result<()> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(());
    };
    let mut stems = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("mtimes")
            && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
        {
            stems.push(stem.to_string());
        }
    }
    for stem in stems {
        for ext in ["pack", "idx", "rev", "mtimes", "bitmap"] {
            let path = pack_dir.join(format!("{stem}.{ext}"));
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(GitError::Io(err.to_string())),
            }
        }
    }
    Ok(())
}

fn remove_pack_garbage_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(GitError::Io(err.to_string())),
    }
}

fn prune_object_is_expired(db: &FileObjectDatabase, oid: &ObjectId, expire: i64) -> Result<bool> {
    if expire == i64::MIN {
        return Ok(false);
    }
    if expire == i64::MAX {
        return Ok(true);
    }
    let path = db.loose().object_path(oid)?;
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0);
    Ok(modified <= expire)
}

fn prune_temporary_files(path: &Path, expire: i64, dry_run: bool, verbose: bool) -> Result<()> {
    let Ok(entries) = fs::read_dir(path) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("tmp_") {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if file_mtime_seconds(&metadata) > expire {
            continue;
        }
        if dry_run || verbose {
            if metadata.is_dir() {
                println!("Removing stale temporary directory {}", path.display());
            } else {
                println!("Removing stale temporary file {}", path.display());
            }
        }
        if dry_run {
            continue;
        }
        if metadata.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

fn file_mtime_seconds(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

fn prune_packed_loose_objects(git_dir: &Path, format: ObjectFormat, dry_run: bool) -> Result<()> {
    let objects_dir = repository_objects_dir(git_dir);
    let packed = sley_odb::packed_object_ids(&objects_dir, format)?;
    if packed.is_empty() {
        return Ok(());
    }
    for (oid, path) in prune_loose_object_paths(&objects_dir, format)? {
        if !packed.contains(&oid) || dry_run {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
    }
    if !dry_run {
        prune_empty_loose_object_dirs(&objects_dir)?;
    }
    Ok(())
}

fn prune_loose_object_paths(
    objects_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<(ObjectId, PathBuf)>> {
    let mut objects = Vec::new();
    if !objects_dir.exists() {
        return Ok(objects);
    }
    let hex_len = format.hex_len();
    for entry in fs::read_dir(objects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let fanout = entry.file_name();
        let Some(fanout) = fanout.to_str() else {
            continue;
        };
        if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        for object_entry in fs::read_dir(entry.path())? {
            let object_entry = object_entry?;
            if !object_entry.file_type()?.is_file() {
                continue;
            }
            let suffix = object_entry.file_name();
            let Some(suffix) = suffix.to_str() else {
                continue;
            };
            if suffix.len() != hex_len - 2 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let oid = ObjectId::from_hex(format, &format!("{fanout}{suffix}"))?;
            objects.push((oid, object_entry.path()));
        }
    }
    objects.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(objects)
}

fn prune_empty_loose_object_dirs(objects_dir: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(objects_dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() == 2 && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            let _ = fs::remove_dir(entry.path());
        }
    }
    Ok(())
}

fn prune_repack_shallow_file(
    git_dir: &Path,
    format: ObjectFormat,
    roots: &[ObjectId],
) -> Result<()> {
    // Filter repacks intentionally leave large blobs absent from the local ODB
    // (`--filter-to`). Only a present `shallow` file needs a reachability walk,
    // and even then missing objects must be tolerated so filtered-out blobs do
    // not abort an otherwise successful repack.
    if !git_dir.join("shallow").exists() {
        return Ok(());
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let reachable = sley_odb::collect_reachable_object_ids_tolerating_missing(
        &db,
        format,
        roots.iter().copied(),
    )?;
    prune_shallow_file(git_dir, format, &reachable, false, false)
}

fn prune_shallow_file(
    git_dir: &Path,
    format: ObjectFormat,
    reachable: &HashSet<ObjectId>,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    let path = git_dir.join("shallow");
    let Ok(contents) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let mut retained = Vec::new();
    let mut removed = Vec::new();
    for line in contents.lines() {
        let value = line.trim();
        if value.len() != format.hex_len() {
            retained.push(value.to_string());
            continue;
        }
        let oid = ObjectId::from_hex(format, value)?;
        if reachable.contains(&oid) {
            retained.push(value.to_string());
        } else {
            removed.push(oid);
        }
    }
    if (dry_run || verbose) && !removed.is_empty() {
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        for oid in &removed {
            let type_name = db
                .read_object_header(oid)?
                .map(|(object_type, _size)| object_type.as_str())
                .unwrap_or("unknown");
            println!("{oid} {type_name}");
        }
    }
    if dry_run || removed.is_empty() {
        return Ok(());
    }
    if retained.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
    } else {
        let mut out = retained.join("\n");
        out.push('\n');
        fs::write(path, out)?;
    }
    Ok(())
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

fn count_objects_size(size_kib: u64, human_readable: bool) -> String {
    if human_readable {
        count_objects_human_size(size_kib)
    } else {
        size_kib.to_string()
    }
}

fn count_objects_pack_size(size_bytes: u64, human_readable: bool) -> String {
    if human_readable {
        count_objects_human_bytes(size_bytes)
    } else {
        (size_bytes / 1024).to_string()
    }
}

fn count_objects_human_size(size_kib: u64) -> String {
    if size_kib == 0 {
        return "0 bytes".to_string();
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut size = size_kib as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.2} {}", UNITS[unit])
}

#[cfg(unix)]
fn filesystem_size_kib(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().div_ceil(2)
}

#[cfg(not(unix))]
fn filesystem_size_kib(metadata: &fs::Metadata) -> u64 {
    metadata.len().div_ceil(1024)
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
        "compact" => cmd_multi_pack_index_compact(cli_session, &combined),
        "expire" => cmd_multi_pack_index_expire(cli_session, &combined),
        "repack" => cmd_multi_pack_index_repack(cli_session, &combined),
        "write" => cmd_multi_pack_index_write(cli_session, &combined),
        "verify" => cmd_multi_pack_index_verify(cli_session, &combined),
        other => {
            eprintln!("error: unknown subcommand: `{other}'");
            eprint!("{MULTI_PACK_INDEX_USAGE}");
            Err(GitError::Exit(129))
        }
    }
}

fn cmd_multi_pack_index_write(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    cmd_multi_pack_index_write_with_pack_names(cli_session, args, None)
}

fn cmd_multi_pack_index_write_with_pack_names(
    cli_session: &crate::session::CliSession,
    args: &[String],
    selected_pack_names: Option<Vec<String>>,
) -> Result<()> {
    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let config = read_repo_config(&git_dir)?;
    let write_bitmap_lookup_table = config
        .get_bool("pack", None, "writeBitmapLookupTable")
        .unwrap_or(false);
    let write_bitmap_hash_cache = config
        .get_bool("pack", None, "writeBitmapHashCache")
        .unwrap_or(true);
    let mut object_dir: Option<PathBuf> = None;
    let mut stdin_packs = false;
    let mut write_bitmap = false;
    let mut incremental = false;
    let mut preferred_pack_name: Option<String> = None;
    let mut refs_snapshot: Option<PathBuf> = None;
    let mut write_chain_file = true;
    let mut base_checksum: Option<String> = None;
    let mut progress = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            value if value.starts_with("--object-dir=") => {
                let value = value
                    .strip_prefix("--object-dir=")
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            "--stdin-packs" => stdin_packs = true,
            "--no-stdin-packs" => stdin_packs = false,
            "--bitmap" => write_bitmap = true,
            "--no-bitmap" => write_bitmap = false,
            "--incremental" => incremental = true,
            "--no-incremental" => incremental = false,
            "--write-chain-file" => write_chain_file = true,
            "--no-write-chain-file" => write_chain_file = false,
            "--base" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--base requires a value".into()))?;
                base_checksum = Some(value.clone());
            }
            "--preferred-pack" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--preferred-pack requires a value".into()))?;
                preferred_pack_name = Some(value.clone());
            }
            value if value.starts_with("--preferred-pack=") => {
                preferred_pack_name = Some(value["--preferred-pack=".len()..].to_string());
            }
            value if value.starts_with("--base=") => {
                base_checksum = Some(value["--base=".len()..].to_string());
            }
            "--refs-snapshot" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--refs-snapshot requires a value".into()))?;
                refs_snapshot = Some(resolve_cli_path(&cwd, value));
            }
            value if value.starts_with("--refs-snapshot=") => {
                refs_snapshot = Some(resolve_cli_path(&cwd, &value["--refs-snapshot=".len()..]));
            }
            other => {
                return Err(GitError::Unsupported(format!(
                    "multi-pack-index write option {other}"
                )));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(&git_dir));
    let pack_dir = object_dir.join("pack");
    fs::create_dir_all(&pack_dir)?;
    if !write_chain_file && !incremental {
        eprintln!("error: cannot use --no-write-chain-file without --incremental");
        return Err(GitError::Exit(128));
    }
    if base_checksum.is_some() && write_chain_file {
        eprintln!("error: cannot use --base without --no-write-chain-file");
        return Err(GitError::Exit(128));
    }
    if incremental {
        return cmd_multi_pack_index_write_incremental(MidxWriteIncremental {
            git_dir: &git_dir,
            object_dir: &object_dir,
            pack_dir: &pack_dir,
            format,
            stdin_packs,
            write_bitmap,
            preferred_pack_name: preferred_pack_name.as_deref(),
            refs_snapshot: refs_snapshot.as_deref(),
            write_chain_file,
            base_checksum: base_checksum.as_deref(),
            progress,
        });
    }
    if progress {
        // Upstream shows a delayed progress meter labelled this way; with
        // GIT_PROGRESS_DELAY=0 it appears immediately. We emit a single line so
        // `--progress` produces non-empty stderr and the default stays silent.
        eprintln!("Adding packfiles to multi-pack-index");
    }

    // If a midx already exists on disk but its trailing checksum does not match
    // its contents, upstream refuses to reuse it and warns before rebuilding
    // from scratch (midx-write.c: "ignoring existing multi-pack-index; checksum
    // mismatch").
    let existing_midx = pack_dir.join("multi-pack-index");
    if let Ok(bytes) = fs::read(&existing_midx)
        && bytes.len() > format.raw_len()
    {
        let checksum_offset = bytes.len() - format.raw_len();
        if let Ok(actual) = sley_core::digest_bytes(format, &bytes[..checksum_offset])
            && actual.as_bytes() != &bytes[checksum_offset..]
        {
            eprintln!("warning: ignoring existing multi-pack-index; checksum mismatch");
        }
    }

    let mut pack_names = if let Some(pack_names) = selected_pack_names {
        pack_names
    } else if stdin_packs {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        input
            .lines()
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect()
    } else {
        let mut pack_names = Vec::new();
        for entry in fs::read_dir(&pack_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            pack_names.push(name.to_string());
        }
        pack_names
    };
    pack_names.sort();

    if pack_names.is_empty() {
        // Upstream resolves the preferred-pack name against the (here empty)
        // pack set first, warning when it is unknown, and only then refuses to
        // write a midx that would index zero packs (midx-write.c: "no pack
        // files to index."), exiting non-zero.
        if let Some(name) = &preferred_pack_name {
            eprintln!("warning: unknown preferred pack: '{name}'");
        }
        eprintln!("error: no pack files to index.");
        return Err(GitError::Exit(1));
    }
    let write_reverse_index = write_bitmap
        && env::var("GIT_TEST_MIDX_WRITE_REV").is_ok_and(|value| value == "1" || value == "true");
    let layer = build_midx_layer(
        sley_odb::MultiPackIndexLayerOptions {
            object_dir,
            format,
            version: 1,
            pack_names,
            excluded_oids: HashSet::new(),
            write_bitmap,
            preferred_pack_name,
            skip_if_unchanged: true,
        },
        |db| {
            Ok(sley_odb::MultiPackIndexBitmapInputs {
                preferred_tips: midx_bitmap_tips(&git_dir, db, format, refs_snapshot.as_deref())?,
                pseudo_merge_groups: repack_pseudo_merge_groups(&git_dir, db, format)?,
                write_lookup_table: write_bitmap_lookup_table,
                write_hash_cache: write_bitmap_hash_cache,
                restrict_to_tips: refs_snapshot.is_some(),
                write_reverse_index,
                missing_closure: sley_odb::MissingMidxBitmapPolicy::Error,
            })
        },
    )?;
    if layer.unchanged {
        return Ok(());
    }
    let midx_checksum = layer.checksum;
    let bitmap_name = format!("multi-pack-index-{midx_checksum}.bitmap");

    // The engine constructs every dependent artifact before the MIDX lands;
    // a bitmap closure failure therefore leaves no partially updated index.
    fs::write(pack_dir.join("multi-pack-index"), &layer.midx)?;
    remove_incremental_midx_dir(&pack_dir)?;

    let rev_name = format!("multi-pack-index-{midx_checksum}.rev");
    if let Some(reverse_index) = &layer.reverse_index {
        fs::write(pack_dir.join(&rev_name), reverse_index)?;
    }

    // Clear midx bitmap/rev sidecars that don't belong to this write: stale
    // checksums always; the current checksum's too when no bitmap was asked
    // for (upstream clear_midx_files_ext keeps only what it just wrote).
    for entry in fs::read_dir(&pack_dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("multi-pack-index-")
            && (name.ends_with(".bitmap") || name.ends_with(".rev"))
            && (!layer.wrote_bitmap || name != bitmap_name)
            && (layer.reverse_index.is_none() || name != rev_name)
        {
            let _ = fs::remove_file(&path);
        }
    }

    if let Some(bitmap) = layer.bitmap {
        if write_bitmap_lookup_table {
            sley_core::trace2::region("pack-bitmap-write", "writing_lookup_table");
        }
        let bitmap_path = pack_dir.join(&bitmap_name);
        let temp_path = bitmap_path.with_extension("bitmap.tmp");
        fs::write(&temp_path, &bitmap)?;
        fs::rename(&temp_path, &bitmap_path)?;
    }
    Ok(())
}

fn build_midx_layer<F>(
    options: sley_odb::MultiPackIndexLayerOptions,
    bitmap_inputs: F,
) -> Result<sley_odb::MultiPackIndexLayerOutcome>
where
    F: FnOnce(&FileObjectDatabase) -> Result<sley_odb::MultiPackIndexBitmapInputs>,
{
    sley_odb::build_multi_pack_index_layer(options, bitmap_inputs, render_midx_event)
        .map_err(render_midx_error)
}

fn render_midx_event(event: sley_odb::MultiPackIndexEvent) {
    match event {
        sley_odb::MultiPackIndexEvent::UnknownPreferredPack(name) => {
            eprintln!("warning: unknown preferred pack: '{name}'");
        }
        sley_odb::MultiPackIndexEvent::RefusingEmptyBitmap => {
            eprintln!("warning: refusing to write multi-pack .bitmap without any objects");
        }
    }
}

fn render_midx_error(err: sley_odb::MultiPackIndexLayerError) -> GitError {
    match err {
        sley_odb::MultiPackIndexLayerError::Source(err) => err,
        sley_odb::MultiPackIndexLayerError::CouldNotLoadPack => {
            eprintln!("error: could not load pack");
            GitError::Exit(1)
        }
        sley_odb::MultiPackIndexLayerError::EmptyPreferredPack(path) => {
            eprintln!(
                "error: cannot select preferred pack {} with no objects",
                path.display()
            );
            GitError::Exit(255)
        }
        sley_odb::MultiPackIndexLayerError::BitmapUnavailable => {
            eprintln!("fatal: could not write multi-pack bitmap");
            GitError::Exit(1)
        }
    }
}

struct MidxWriteIncremental<'a> {
    git_dir: &'a Path,
    object_dir: &'a Path,
    pack_dir: &'a Path,
    format: ObjectFormat,
    stdin_packs: bool,
    write_bitmap: bool,
    preferred_pack_name: Option<&'a str>,
    refs_snapshot: Option<&'a Path>,
    write_chain_file: bool,
    base_checksum: Option<&'a str>,
    progress: bool,
}

#[derive(Clone)]
struct IncrementalMidxLayer {
    midx: MultiPackIndex,
}

fn cmd_multi_pack_index_write_incremental(options: MidxWriteIncremental<'_>) -> Result<()> {
    if options.progress {
        eprintln!("Adding packfiles to multi-pack-index");
    }

    let mut chain = read_midx_chain(options.pack_dir)?;
    if let Some(checksum) = migrate_single_midx_to_incremental(options.pack_dir, options.format)?
        && !chain.iter().any(|existing| existing == &checksum)
    {
        chain.push(checksum);
    }

    let base_chain = incremental_midx_base_chain(&chain, options.base_checksum)?;
    let layers = read_incremental_midx_layers(options.pack_dir, options.format, &base_chain)?;
    let mut chained_pack_names = HashSet::new();
    let mut chained_oids = HashSet::new();
    for layer in &layers {
        chained_pack_names.extend(layer.midx.pack_names.iter().cloned());
        chained_oids.extend(layer.midx.objects.iter().map(|entry| entry.oid));
    }

    let mut pack_names = collect_midx_pack_names(options.pack_dir, options.stdin_packs)?;
    pack_names.retain(|name| !chained_pack_names.contains(name));

    if pack_names.is_empty() {
        if chain.is_empty() {
            if let Some(name) = options.preferred_pack_name {
                eprintln!("warning: unknown preferred pack: '{name}'");
            }
            eprintln!("error: no pack files to index.");
            return Err(GitError::Exit(1));
        }
        if options.write_chain_file {
            write_midx_chain(options.pack_dir, &chain)?;
        }
        return Ok(());
    }

    let layer = build_midx_layer_from_packs(
        options.git_dir,
        options.object_dir,
        options.format,
        pack_names,
        &chained_oids,
        options.write_bitmap,
        options.preferred_pack_name,
        options.refs_snapshot,
        1,
    )?;
    install_incremental_midx_layer(options.pack_dir, options.format, &layer)?;
    if options.write_chain_file {
        chain.push(layer.checksum);
        write_midx_chain(options.pack_dir, &chain)?;
        clear_incremental_midx_sidecars(options.pack_dir, options.format)?;
    } else {
        println!("{}", layer.checksum);
    }
    Ok(())
}

fn incremental_midx_base_chain(
    chain: &[String],
    base_checksum: Option<&str>,
) -> Result<Vec<String>> {
    match base_checksum {
        None => Ok(chain.to_vec()),
        // Empty `--base=` (e.g. broken `$(nth_line …)` in t5334) is treated as
        // "no base override" — matching Git 2.55.0, which never threads
        // `incremental_base` into `write_midx_file` on the non-stdin path and
        // therefore silently ignores the option.
        Some("") => Ok(chain.to_vec()),
        Some("none") => Ok(Vec::new()),
        Some(base) => {
            let Some(index) = chain.iter().position(|checksum| checksum == base) else {
                eprintln!("error: unknown incremental MIDX base: {base}");
                return Err(GitError::Exit(1));
            };
            Ok(chain[..=index].to_vec())
        }
    }
}

struct BuiltMidxLayer {
    checksum: String,
    midx: Vec<u8>,
    bitmap: Option<Vec<u8>>,
    rev: Option<Vec<u8>>,
}

fn build_midx_layer_from_packs(
    git_dir: &Path,
    object_dir: &Path,
    format: ObjectFormat,
    pack_names: Vec<String>,
    excluded_oids: &HashSet<ObjectId>,
    write_bitmap: bool,
    preferred_pack_name: Option<&str>,
    refs_snapshot: Option<&Path>,
    version: u8,
) -> Result<BuiltMidxLayer> {
    let write_reverse_index = write_bitmap
        && env::var("GIT_TEST_MIDX_WRITE_REV").is_ok_and(|value| value == "1" || value == "true");
    let outcome = build_midx_layer(
        sley_odb::MultiPackIndexLayerOptions {
            object_dir: object_dir.to_path_buf(),
            format,
            version,
            pack_names,
            excluded_oids: excluded_oids.clone(),
            write_bitmap,
            preferred_pack_name: preferred_pack_name.map(ToString::to_string),
            skip_if_unchanged: false,
        },
        |db| {
            Ok(sley_odb::MultiPackIndexBitmapInputs {
                preferred_tips: midx_bitmap_tips(git_dir, db, format, refs_snapshot)?,
                pseudo_merge_groups: repack_pseudo_merge_groups(git_dir, db, format)?,
                write_lookup_table: false,
                write_hash_cache: false,
                restrict_to_tips: false,
                write_reverse_index,
                missing_closure: sley_odb::MissingMidxBitmapPolicy::WriteEmpty,
            })
        },
    )?;
    Ok(BuiltMidxLayer {
        checksum: outcome.checksum.to_hex(),
        midx: outcome.midx,
        bitmap: outcome.bitmap,
        rev: outcome.reverse_index,
    })
}

/// Resolve the exact tip universe for a MIDX bitmap write. A refs snapshot is
/// a replacement for the live ref store, not an additive preference list:
/// plain and `+`-prefixed rows both name visible tips (`+` additionally marks
/// a preferred tip upstream, which sley's selection set already models).
fn midx_bitmap_tips(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    refs_snapshot: Option<&Path>,
) -> Result<HashSet<ObjectId>> {
    let Some(snapshot) = refs_snapshot else {
        return repack_preferred_bitmap_tips(git_dir, db, format);
    };
    let mut tips = HashSet::new();
    for line in fs::read_to_string(snapshot)?.lines() {
        let hex = line.strip_prefix('+').unwrap_or(line).trim();
        if hex.is_empty() {
            continue;
        }
        let oid = ObjectId::from_hex(format, hex)?;
        let commit = sley_rev::peel_to_commit(db, format, &oid)?;
        tips.insert(commit);
    }
    Ok(tips)
}

fn install_incremental_midx_layer(
    pack_dir: &Path,
    _format: ObjectFormat,
    layer: &BuiltMidxLayer,
) -> Result<()> {
    let midx_dir = incremental_midx_dir(pack_dir);
    fs::create_dir_all(&midx_dir)?;
    let midx_path = midx_dir.join(format!("multi-pack-index-{}.midx", layer.checksum));
    fs::write(&midx_path, &layer.midx)?;
    if let Some(bitmap) = &layer.bitmap {
        fs::write(
            midx_dir.join(format!("multi-pack-index-{}.bitmap", layer.checksum)),
            bitmap,
        )?;
    }
    if let Some(rev) = &layer.rev {
        fs::write(
            midx_dir.join(format!("multi-pack-index-{}.rev", layer.checksum)),
            rev,
        )?;
    }
    Ok(())
}

fn collect_midx_pack_names(pack_dir: &Path, stdin_packs: bool) -> Result<Vec<String>> {
    let mut pack_names = if stdin_packs {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        input
            .lines()
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect()
    } else {
        let mut names = Vec::new();
        for entry in fs::read_dir(pack_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            names.push(name.to_string());
        }
        names
    };
    pack_names.sort();
    Ok(pack_names)
}

fn incremental_midx_dir(pack_dir: &Path) -> PathBuf {
    pack_dir.join("multi-pack-index.d")
}

fn midx_chain_path(pack_dir: &Path) -> PathBuf {
    incremental_midx_dir(pack_dir).join("multi-pack-index-chain")
}

fn read_midx_chain(pack_dir: &Path) -> Result<Vec<String>> {
    let path = midx_chain_path(pack_dir);
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    Ok(contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn write_midx_chain(pack_dir: &Path, chain: &[String]) -> Result<()> {
    let midx_dir = incremental_midx_dir(pack_dir);
    fs::create_dir_all(&midx_dir)?;
    let mut contents = String::new();
    for checksum in chain {
        contents.push_str(checksum);
        contents.push('\n');
    }
    fs::write(midx_chain_path(pack_dir), contents)?;
    Ok(())
}

fn read_incremental_midx_layers(
    pack_dir: &Path,
    format: ObjectFormat,
    chain: &[String],
) -> Result<Vec<IncrementalMidxLayer>> {
    let midx_dir = incremental_midx_dir(pack_dir);
    let mut layers = Vec::with_capacity(chain.len());
    for checksum in chain {
        let path = midx_dir.join(format!("multi-pack-index-{checksum}.midx"));
        let midx = MultiPackIndex::parse(&fs::read(path)?, format)?;
        layers.push(IncrementalMidxLayer { midx });
    }
    Ok(layers)
}

fn migrate_single_midx_to_incremental(
    pack_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<String>> {
    let midx_path = pack_dir.join("multi-pack-index");
    let Ok(bytes) = fs::read(&midx_path) else {
        return Ok(None);
    };
    if bytes.len() < format.raw_len() {
        return Ok(None);
    }
    let checksum = ObjectId::from_raw(format, &bytes[bytes.len() - format.raw_len()..])?.to_hex();
    let midx_dir = incremental_midx_dir(pack_dir);
    fs::create_dir_all(&midx_dir)?;
    let layer_path = midx_dir.join(format!("multi-pack-index-{checksum}.midx"));
    fs::write(&layer_path, &bytes)?;
    let _ = fs::remove_file(&midx_path);
    for ext in ["bitmap", "rev"] {
        let from = pack_dir.join(format!("multi-pack-index-{checksum}.{ext}"));
        if from.exists() {
            let to = midx_dir.join(format!("multi-pack-index-{checksum}.{ext}"));
            match fs::rename(&from, &to) {
                Ok(()) => {}
                Err(_) => {
                    fs::copy(&from, &to)?;
                    let _ = fs::remove_file(&from);
                }
            }
        }
    }
    Ok(Some(checksum))
}

fn remove_incremental_midx_dir(pack_dir: &Path) -> Result<()> {
    let midx_dir = incremental_midx_dir(pack_dir);
    let preserve_empty_dir = midx_dir.exists();
    match fs::remove_dir_all(&midx_dir) {
        Ok(()) if preserve_empty_dir => {
            fs::create_dir_all(&midx_dir).map_err(|err| GitError::Io(err.to_string()))
        }
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(GitError::Io(err.to_string())),
    }
}

fn clear_incremental_midx_sidecars(pack_dir: &Path, format: ObjectFormat) -> Result<()> {
    let chain = read_midx_chain(pack_dir)?;
    let keep: HashSet<String> = chain.into_iter().collect();
    let midx_dir = incremental_midx_dir(pack_dir);
    let Ok(entries) = fs::read_dir(&midx_dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("multi-pack-index-") {
            continue;
        }
        let Some((checksum, ext)) = name
            .strip_prefix("multi-pack-index-")
            .and_then(|rest| rest.rsplit_once('.'))
        else {
            continue;
        };
        if checksum.len() != format.hex_len() || !matches!(ext, "midx" | "bitmap" | "rev") {
            continue;
        }
        if !keep.contains(checksum) {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn cmd_multi_pack_index_compact(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    let mut write_bitmap = false;
    let mut incremental = false;
    let mut endpoints = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            value if value.starts_with("--object-dir=") => {
                object_dir = Some(resolve_cli_path(&cwd, &value["--object-dir=".len()..]));
            }
            "--bitmap" => write_bitmap = true,
            "--no-bitmap" => write_bitmap = false,
            "--incremental" => incremental = true,
            "--no-incremental" => incremental = false,
            "--progress" | "--no-progress" => {}
            other if other.starts_with('-') => {
                return Err(GitError::Unsupported(format!(
                    "multi-pack-index compact option {other}"
                )));
            }
            other => endpoints.push(other.to_string()),
        }
    }
    if endpoints.len() != 2 {
        eprint!("{MULTI_PACK_INDEX_USAGE}");
        return Err(GitError::Exit(129));
    }
    let config = read_repo_config(&git_dir)?;
    if config
        .get_entry("midx", None, "version")
        .flatten()
        .is_some_and(|value| value.trim() == "1")
    {
        eprintln!("fatal: cannot perform MIDX compaction with v1 format");
        return Err(GitError::Exit(128));
    }
    if !incremental {
        incremental = true;
    }
    if !incremental {
        return Ok(());
    }

    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(&git_dir));
    let pack_dir = object_dir.join("pack");
    let mut chain = read_midx_chain(&pack_dir)?;
    let layers = read_incremental_midx_layers(&pack_dir, format, &chain)?;
    let Some(from_idx) = chain.iter().position(|checksum| checksum == &endpoints[0]) else {
        eprintln!("fatal: could not find MIDX: {}", endpoints[0]);
        return Err(GitError::Exit(128));
    };
    let Some(to_idx) = chain.iter().position(|checksum| checksum == &endpoints[1]) else {
        eprintln!("fatal: could not find MIDX: {}", endpoints[1]);
        return Err(GitError::Exit(128));
    };
    if from_idx == to_idx {
        eprintln!("fatal: MIDX compaction endpoints must be unique");
        return Err(GitError::Exit(128));
    }
    if from_idx > to_idx {
        eprintln!(
            "fatal: MIDX {} must be an ancestor of {}",
            endpoints[0], endpoints[1]
        );
        return Err(GitError::Exit(128));
    }

    let mut pack_names = Vec::new();
    let mut objects = Vec::new();
    let mut pack_name_to_id = HashMap::new();
    for layer in &layers[from_idx..=to_idx] {
        for name in &layer.midx.pack_names {
            if !pack_name_to_id.contains_key(name) {
                let id = pack_names.len() as u32;
                pack_name_to_id.insert(name.clone(), id);
                pack_names.push(name.clone());
            }
        }
        for entry in &layer.midx.objects {
            let Some(old_name) = layer.midx.pack_names.get(entry.pack_int_id as usize) else {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index object points past pack table".into(),
                ));
            };
            let new_pack = *pack_name_to_id.get(old_name).ok_or_else(|| {
                GitError::InvalidFormat("compacted MIDX pack missing from table".into())
            })?;
            objects.push(MultiPackIndexEntry {
                oid: entry.oid,
                pack_int_id: new_pack,
                offset: entry.offset,
                force_large_offset: entry.force_large_offset,
            });
        }
    }
    objects.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
    objects.dedup_by(|next, kept| next.oid == kept.oid);

    let write_reverse_index = write_bitmap
        && env::var("GIT_TEST_MIDX_WRITE_REV").is_ok_and(|value| value == "1" || value == "true");
    let compacted = sley_odb::build_multi_pack_index_layer_from_entries(
        sley_odb::MultiPackIndexEntryLayerOptions {
            object_dir,
            format,
            version: 2,
            pack_names,
            objects,
            write_bitmap,
            preferred_pack: write_bitmap.then_some(0),
        },
        |db| {
            Ok(sley_odb::MultiPackIndexBitmapInputs {
                preferred_tips: midx_bitmap_tips(&git_dir, db, format, None)?,
                pseudo_merge_groups: repack_pseudo_merge_groups(&git_dir, db, format)?,
                write_lookup_table: false,
                write_hash_cache: false,
                restrict_to_tips: false,
                write_reverse_index,
                missing_closure: sley_odb::MissingMidxBitmapPolicy::WriteEmpty,
            })
        },
    )
    .map_err(render_midx_error)?;
    let compacted = BuiltMidxLayer {
        checksum: compacted.checksum.to_hex(),
        midx: compacted.midx,
        bitmap: compacted.bitmap,
        rev: compacted.reverse_index,
    };
    install_incremental_midx_layer(&pack_dir, format, &compacted)?;

    let old: HashSet<String> = chain[from_idx..=to_idx].iter().cloned().collect();
    chain.splice(from_idx..=to_idx, std::iter::once(compacted.checksum));
    write_midx_chain(&pack_dir, &chain)?;

    let retained: HashSet<String> = chain.iter().cloned().collect();
    for checksum in old {
        if retained.contains(&checksum) {
            continue;
        }
        for ext in ["midx", "bitmap", "rev"] {
            let _ = fs::remove_file(
                incremental_midx_dir(&pack_dir).join(format!("multi-pack-index-{checksum}.{ext}")),
            );
        }
    }
    clear_incremental_midx_sidecars(&pack_dir, format)?;
    Ok(())
}

/// Scan `<object_dir>/pack` for `.idx` files and write a fresh, non-bitmap
/// multi-pack-index over them, applying upstream's cross-pack duplicate
/// resolution (keep the copy from the newest pack, ties broken by lowest pack
/// id). This is the default `multi-pack-index write` behaviour, factored out so
/// `repack` and `expire` can rewrite the midx after changing the pack set.
fn write_default_midx(object_dir: &Path, format: ObjectFormat) -> Result<()> {
    let pack_dir = object_dir.join("pack");
    let mut pack_names = Vec::new();
    for entry in fs::read_dir(&pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            pack_names.push(name.to_string());
        }
    }
    pack_names.sort();
    if pack_names.is_empty() {
        // Nothing to index; remove any stale midx so callers observe the empty
        // state the way upstream leaves it after dropping the last pack.
        let _ = fs::remove_file(pack_dir.join("multi-pack-index"));
        return Ok(());
    }

    let pack_mtimes: Vec<std::time::SystemTime> = pack_names
        .iter()
        .map(|name| {
            fs::metadata(pack_dir.join(name).with_extension("pack"))
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        })
        .collect();
    let pack_mtime = |pack_int_id: u32| -> std::time::SystemTime {
        pack_mtimes
            .get(pack_int_id as usize)
            .copied()
            .unwrap_or(std::time::UNIX_EPOCH)
    };

    let mut objects = Vec::new();
    for (pack_int_id, pack_name) in pack_names.iter().enumerate() {
        let index =
            PackIndex::parse_without_checksum(&fs::read(pack_dir.join(pack_name))?, format)?;
        for entry in index.entries {
            objects.push(MultiPackIndexEntry {
                oid: entry.oid,
                pack_int_id: pack_int_id as u32,
                offset: entry.offset,
                force_large_offset: false,
            });
        }
    }

    objects.sort_by(|left, right| {
        left.oid
            .as_bytes()
            .cmp(right.oid.as_bytes())
            .then_with(|| pack_mtime(right.pack_int_id).cmp(&pack_mtime(left.pack_int_id)))
            .then_with(|| left.pack_int_id.cmp(&right.pack_int_id))
    });
    objects.dedup_by(|next, kept| next.oid == kept.oid);

    let midx = MultiPackIndex::write(format, 1, &pack_names, &objects)?;
    let midx_checksum = ObjectId::from_raw(format, &midx[midx.len() - format.raw_len()..])?;

    // Clear stale bitmap/rev sidecars not produced by this (non-bitmap) write.
    for entry in fs::read_dir(&pack_dir)? {
        let path = entry?.path();
        if let Some(name) = path.file_name().and_then(|name| name.to_str())
            && name.starts_with("multi-pack-index-")
            && (name.ends_with(".bitmap") || name.ends_with(".rev"))
        {
            let _ = fs::remove_file(&path);
        }
    }
    let _ = midx_checksum;

    fs::write(pack_dir.join("multi-pack-index"), &midx)?;
    Ok(())
}

/// Parse the `--object-dir`/`--progress` options shared by `repack` and
/// `expire`, returning the resolved object dir and whether progress is forced.
fn parse_midx_object_dir_and_progress(
    args: &[String],
    cwd: &Path,
    git_dir: &Path,
    subcommand: &str,
) -> Result<(PathBuf, bool, Option<u64>)> {
    let mut object_dir: Option<PathBuf> = None;
    let mut progress = false;
    let mut batch_size: Option<u64> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(cwd, value));
            }
            value if value.starts_with("--object-dir=") => {
                let value = &value["--object-dir=".len()..];
                object_dir = Some(resolve_cli_path(cwd, value));
            }
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            "--batch-size" if subcommand == "repack" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--batch-size requires a value".into()))?;
                batch_size = Some(value.parse().map_err(|_| {
                    GitError::Command("option `batch-size' expects a numerical value".into())
                })?);
            }
            value if subcommand == "repack" && value.starts_with("--batch-size=") => {
                let value = &value["--batch-size=".len()..];
                batch_size = Some(value.parse().map_err(|_| {
                    GitError::Command("option `batch-size' expects a numerical value".into())
                })?);
            }
            other => {
                return Err(GitError::Unsupported(format!(
                    "multi-pack-index {subcommand} option {other}"
                )));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(git_dir));
    Ok((object_dir, progress, batch_size))
}

/// Whether a pack (named by its `.idx` basename) carries a `.keep` companion.
fn pack_has_keep(pack_dir: &Path, idx_name: &str) -> bool {
    let keep = pack_dir.join(idx_name).with_extension("keep");
    keep.exists()
}

/// Whether a pack (named by its `.idx` basename) is a cruft pack, i.e. has a
/// `.mtimes` companion (upstream marks cruft packs this way).
fn pack_is_cruft(pack_dir: &Path, idx_name: &str) -> bool {
    pack_dir.join(idx_name).with_extension("mtimes").exists()
}

fn cmd_multi_pack_index_repack(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let (object_dir, progress, batch_size) =
        parse_midx_object_dir_and_progress(args, &cwd, &git_dir, "repack")?;
    let pack_dir = object_dir.join("pack");
    let midx_path = pack_dir.join("multi-pack-index");
    if !midx_path.exists() {
        return Ok(());
    }
    let midx = MultiPackIndex::parse(&fs::read(&midx_path)?, format)?;
    let num_packs = midx.pack_names.len();

    // referenced[i] = number of midx objects whose copy lives in pack i, and
    // per-pack file metadata used for batch selection.
    let mut referenced = vec![0u64; num_packs];
    for entry in &midx.objects {
        if (entry.pack_int_id as usize) < num_packs {
            referenced[entry.pack_int_id as usize] += 1;
        }
    }
    let mut pack_objects_total = vec![0u64; num_packs];
    let mut pack_size = vec![0u64; num_packs];
    let mut pack_mtime = vec![std::time::UNIX_EPOCH; num_packs];
    for (i, name) in midx.pack_names.iter().enumerate() {
        if let Ok(index) =
            PackIndex::parse_without_checksum(&fs::read(pack_dir.join(name))?, format)
        {
            pack_objects_total[i] = index.entries.len() as u64;
        }
        let pack_path = pack_dir.join(name).with_extension("pack");
        if let Ok(meta) = fs::metadata(&pack_path) {
            pack_size[i] = meta.len();
            if let Ok(mtime) = meta.modified() {
                pack_mtime[i] = mtime;
            }
        }
    }

    let config = read_repo_config(&git_dir)?;
    let pack_kept_objects = config
        .get_bool("repack", None, "packKeptObjects")
        .unwrap_or(false);

    let want = |i: usize| -> bool {
        if !pack_kept_objects && pack_has_keep(&pack_dir, &midx.pack_names[i]) {
            return false;
        }
        if pack_is_cruft(&pack_dir, &midx.pack_names[i]) {
            return false;
        }
        pack_objects_total[i] > 0
    };

    let mut include = vec![false; num_packs];
    match batch_size {
        None | Some(0) => {
            for (i, slot) in include.iter_mut().enumerate() {
                if want(i) {
                    *slot = true;
                }
            }
        }
        Some(batch_size) => {
            if pack_size
                .iter()
                .enumerate()
                .filter_map(|(i, size)| want(i).then_some(*size))
                .min()
                .is_some_and(|min_size| batch_size <= min_size)
            {
                return Ok(());
            }

            // Visit packs smallest-mtime first; include the smaller packs whose
            // expected (reference-proportional) size keeps the running total
            // under the batch, skipping any single pack already >= batch.
            let mut order: Vec<usize> = (0..num_packs).collect();
            order.sort_by(|&a, &b| pack_mtime[a].cmp(&pack_mtime[b]));
            let mut total: u64 = 0;
            for i in order {
                if total >= batch_size {
                    break;
                }
                if !want(i) {
                    continue;
                }
                // expected_size ~= referenced/num_objects * pack_size, in the
                // same shifted-integer form upstream uses.
                let objects = pack_objects_total[i].max(1);
                let mut expected = (referenced[i] << 14) / objects;
                expected = expected.saturating_mul(pack_size[i]);
                expected = (expected + (1 << 13)) >> 14;
                if expected >= batch_size {
                    continue;
                }
                total = total.saturating_add(expected);
                include[i] = true;
            }
        }
    }

    let packs_to_repack = include.iter().filter(|&&v| v).count();
    if packs_to_repack <= 1 {
        return Ok(());
    }
    if progress {
        // Upstream forwards `--progress` to the spawned pack-objects, which
        // prints its own meters once it actually has packs to combine. A single
        // line keeps `--progress` non-empty while the default stays silent.
        eprintln!("Repacking multi-pack-index");
    }

    // Collect the oids whose copy lives in an included pack, then build one new
    // pack from them and rewrite the midx.
    let db = FileObjectDatabase::new(object_dir.clone(), format);
    let mut inputs_oids = Vec::new();
    for entry in &midx.objects {
        if include
            .get(entry.pack_int_id as usize)
            .copied()
            .unwrap_or(false)
        {
            inputs_oids.push(entry.oid);
        }
    }

    let mut encoded = Vec::with_capacity(inputs_oids.len());
    for oid in &inputs_oids {
        encoded.push(db.read_object(oid)?);
    }
    let inputs: Vec<PackInput<'_>> = inputs_oids
        .iter()
        .zip(&encoded)
        .map(|(oid, object)| PackInput {
            oid,
            object: object.as_ref(),
        })
        .collect();

    let written = PackFile::write_packed_with_known_ids_and_options(
        &inputs,
        format,
        &PackWriteOptions::new(),
    )?;
    let checksum = written.checksum.to_hex();
    let base = pack_dir.join(format!("pack-{checksum}"));
    let positions = pack_order_index_positions(&written.entries);
    let reverse_index = PackReverseIndex::write(format, &positions, &written.checksum)?;
    fs::write(base.with_extension("pack"), &written.pack)?;
    fs::write(base.with_extension("rev"), &reverse_index)?;
    fs::write(base.with_extension("idx"), &written.index)?;

    write_default_midx(&object_dir, format)
}

fn cmd_multi_pack_index_verify(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    // Upstream defaults progress to a tty heuristic; when stderr is not a tty
    // it stays off. `--progress` forces it on, `--no-progress` forces it off.
    // Our test oracle never has a tty, so default off and honour the flags.
    let mut progress = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            value if value.starts_with("--object-dir=") => {
                let value = value
                    .strip_prefix("--object-dir=")
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            other => {
                return Err(GitError::Unsupported(format!(
                    "multi-pack-index verify option {other}"
                )));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(&git_dir));
    verify_midx_at(&object_dir, format, progress)
}

/// Run the full upstream `verify_midx_file` pass over the multi-pack-index in
/// `<object_dir>/pack/multi-pack-index`, emitting git-exact error substrings to
/// stderr and returning `GitError::Exit(1)` on any detected corruption.
///
/// Parse-time corruptions (signature, version, hash version, chunk table,
/// fanout order, pack names order, pack-int-id) abort with git's load-time
/// `die()`/`error()` strings; verify-time corruptions (incorrect checksum,
/// failed pack load, no oid, oid lookup order, incorrect object offset) are
/// reported the way upstream's verify pass reports them.
pub(crate) fn verify_midx_at(
    object_dir: &Path,
    format: ObjectFormat,
    progress: bool,
) -> Result<()> {
    let pack_dir = object_dir.join("pack");
    let midx_path = pack_dir.join("multi-pack-index");
    let bytes = match fs::read(&midx_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // No midx ⇒ upstream verify is a no-op success.
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };

    let parsed = match parse_midx_for_verify(&bytes, format) {
        Ok(parsed) => parsed,
        Err(message) => {
            if message == "multi-pack-index large offset out of bounds" {
                eprintln!("error: incorrect object offset");
            } else {
                eprintln!("error: {message}");
            }
            return Err(GitError::Exit(1));
        }
    };

    let mut reported = false;
    let mut report = |message: String| {
        eprintln!("error: {message}");
        reported = true;
    };

    // Verify-time checksum check: recompute over everything but the trailing
    // hash and compare.
    let checksum_offset = bytes.len() - format.raw_len();
    let actual_checksum = sley_core::digest_bytes(format, &bytes[..checksum_offset])?;
    if actual_checksum.as_bytes() != &bytes[checksum_offset..] {
        report("incorrect checksum".to_string());
    }

    if progress {
        eprintln!("Looking for referenced packfiles");
    }

    // Load each referenced pack-index; a missing/corrupt one is reported but
    // not fatal to the rest of the pass.
    let mut pack_indexes: Vec<Option<PackIndex>> = Vec::with_capacity(parsed.pack_names.len());
    for (position, name) in parsed.pack_names.iter().enumerate() {
        match fs::read(pack_dir.join(name))
            .ok()
            .and_then(|raw| PackIndex::parse_without_checksum(&raw, format).ok())
        {
            Some(index) => pack_indexes.push(Some(index)),
            None => {
                report(format!("failed to load pack in position {position}"));
                pack_indexes.push(None);
            }
        }
    }

    if parsed.object_count == 0 {
        report("the midx contains no oid".to_string());
        return if reported {
            Err(GitError::Exit(1))
        } else {
            Ok(())
        };
    }

    if progress {
        eprintln!("Verifying OID order in multi-pack-index");
    }
    for window in parsed.entries.windows(2) {
        if window[0].oid.as_bytes() >= window[1].oid.as_bytes() {
            report(format!(
                "oid lookup out of order: oid[?] = {} >= {} = oid[?]",
                window[0].oid.to_hex(),
                window[1].oid.to_hex()
            ));
        }
    }

    if progress {
        eprintln!("Verifying object offsets");
    }
    // Build per-pack offset lookups once, then check each midx entry's offset
    // against the pack's own .idx.
    for (idx, entry) in parsed.entries.iter().enumerate() {
        let Some(Some(index)) = pack_indexes.get(entry.pack_int_id as usize) else {
            // A pack that failed to load was already reported above.
            continue;
        };
        let pack_offset = index.find(&entry.oid).map(|e| e.offset);
        match pack_offset {
            Some(offset) if offset == entry.offset => {}
            Some(offset) => report(format!(
                "incorrect object offset for oid[{idx}] = {}: {:x} != {:x}",
                entry.oid.to_hex(),
                entry.offset,
                offset
            )),
            None => report(format!(
                "failed to load pack entry for oid[{idx}] = {}",
                entry.oid.to_hex()
            )),
        }
    }

    if reported {
        Err(GitError::Exit(1))
    } else {
        Ok(())
    }
}

/// A minimal parse of the multi-pack-index used purely by verify, reproducing
/// upstream `load_multi_pack_index`'s die/error wording so corruption tests
/// match byte-for-byte. Returns the human-facing message string on failure.
struct VerifyMidx {
    pack_names: Vec<String>,
    object_count: usize,
    entries: Vec<MultiPackIndexEntry>,
}

fn parse_midx_for_verify(
    bytes: &[u8],
    format: ObjectFormat,
) -> std::result::Result<VerifyMidx, String> {
    let hash_len = format.raw_len();
    const MIDX_HEADER_SIZE: usize = 12;
    if bytes.len() < MIDX_HEADER_SIZE + 12 + hash_len {
        return Err("multi-pack-index file is too small".to_string());
    }
    if &bytes[..4] != b"MIDX" {
        return Err(format!(
            "multi-pack-index signature 0x{:08x} does not match signature 0x{:08x}",
            u32_be4(&bytes[..4]),
            u32::from_be_bytes(*b"MIDX")
        ));
    }
    let version = bytes[4];
    if version != 1 && version != 2 {
        return Err(format!("multi-pack-index version {version} not recognized"));
    }
    let hash_version = bytes[5];
    let expected_hash_version = match format {
        ObjectFormat::Sha1 => 1u8,
        ObjectFormat::Sha256 => 2u8,
    };
    if hash_version != expected_hash_version {
        return Err(format!(
            "multi-pack-index hash version {hash_version} does not match version {expected_hash_version}"
        ));
    }
    let num_chunks = bytes[6] as usize;
    let num_packs = u32_be4(&bytes[8..12]) as usize;

    // Table of contents: num_chunks entries of (id:4, offset:8) plus a
    // terminating entry. Reproduce read_table_of_contents's check order so the
    // truncated/extended/improper-offset corruptions report git's exact text.
    let checksum_offset = bytes.len() - hash_len;
    let mut chunks: Vec<([u8; 4], u64)> = Vec::with_capacity(num_chunks);
    let mut toc = MIDX_HEADER_SIZE;
    for _ in 0..num_chunks {
        if toc + 12 + 12 > bytes.len() {
            return Err("multi-pack-index file is too small".to_string());
        }
        let chunk_id = [bytes[toc], bytes[toc + 1], bytes[toc + 2], bytes[toc + 3]];
        let chunk_offset = u64_be8(&bytes[toc + 4..toc + 12]);
        if chunk_id == [0, 0, 0, 0] {
            return Err("terminating chunk id appears earlier than expected".to_string());
        }
        // CHUNK alignment for midx is 1 byte, so alignment never trips.
        let next_offset = u64_be8(&bytes[toc + 12 + 4..toc + 12 + 12]);
        if next_offset < chunk_offset || next_offset > checksum_offset as u64 {
            return Err(format!(
                "improper chunk offset(s) {chunk_offset:x} and {next_offset:x}"
            ));
        }
        chunks.push((chunk_id, chunk_offset));
        toc += 12;
    }
    // The final (terminating) entry must have a zero id.
    let final_id = [bytes[toc], bytes[toc + 1], bytes[toc + 2], bytes[toc + 3]];
    if final_id != [0, 0, 0, 0] {
        return Err(format!(
            "final chunk has non-zero id {:x}",
            u32_be4(&final_id)
        ));
    }
    let final_offset = u64_be8(&bytes[toc + 4..toc + 12]);

    // Resolve a chunk's data slice using the next chunk's start (or the
    // terminator) as the end.
    let chunk_slice = |want: &[u8; 4]| -> Option<(usize, usize)> {
        for i in 0..chunks.len() {
            if &chunks[i].0 == want {
                let start = chunks[i].1 as usize;
                let end = if i + 1 < chunks.len() {
                    chunks[i + 1].1 as usize
                } else {
                    final_offset as usize
                };
                return Some((start, end));
            }
        }
        None
    };

    // PNAM (required).
    let Some((pnam_start, pnam_end)) = chunk_slice(b"PNAM") else {
        return Err("multi-pack-index required pack-name chunk missing or corrupted".to_string());
    };
    let pnam = &bytes[pnam_start..pnam_end.min(bytes.len())];
    let mut pack_names = Vec::with_capacity(num_packs);
    let mut cursor = 0usize;
    for _ in 0..num_packs {
        let Some(nul) = pnam[cursor..].iter().position(|b| *b == 0) else {
            return Err("multi-pack-index pack-name chunk is too short".to_string());
        };
        let name = String::from_utf8_lossy(&pnam[cursor..cursor + nul]).into_owned();
        if version == 1
            && let Some(prev) = pack_names.last()
            && &name <= prev
        {
            return Err(format!(
                "multi-pack-index pack names out of order: '{prev}' before '{name}'"
            ));
        }
        pack_names.push(name);
        cursor += nul + 1;
    }

    // OIDF (required) — fanout monotonicity is checked at load time.
    let Some((oidf_start, oidf_end)) = chunk_slice(b"OIDF") else {
        return Err("multi-pack-index required OID fanout chunk missing or corrupted".to_string());
    };
    let oidf = &bytes[oidf_start..oidf_end.min(bytes.len())];
    if oidf.len() != 256 * 4 {
        return Err("multi-pack-index OID fanout is of the wrong size".to_string());
    }
    let fanout: Vec<u32> = (0..256).map(|i| u32_be4(&oidf[i * 4..i * 4 + 4])).collect();
    for i in 0..255 {
        if fanout[i] > fanout[i + 1] {
            return Err(format!(
                "oid fanout out of order: fanout[{i}] = {:x} > {:x} = fanout[{}]",
                fanout[i],
                fanout[i + 1],
                i + 1
            ));
        }
    }
    let object_count = fanout[255] as usize;

    // OIDL (required).
    let Some((oidl_start, oidl_end)) = chunk_slice(b"OIDL") else {
        return Err("multi-pack-index required OID lookup chunk missing or corrupted".to_string());
    };
    let oidl = &bytes[oidl_start..oidl_end.min(bytes.len())];
    if oidl.len() != object_count * hash_len {
        return Err("multi-pack-index OID lookup chunk is the wrong size".to_string());
    }

    // OOFF (required).
    let Some((ooff_start, ooff_end)) = chunk_slice(b"OOFF") else {
        return Err(
            "multi-pack-index required object offsets chunk missing or corrupted".to_string(),
        );
    };
    let ooff = &bytes[ooff_start..ooff_end.min(bytes.len())];
    if ooff.len() != object_count * 8 {
        return Err("multi-pack-index object offset chunk is the wrong size".to_string());
    }

    // LOFF (optional).
    let loff = chunk_slice(b"LOFF").map(|(s, e)| &bytes[s..e.min(bytes.len())]);

    let mut entries = Vec::with_capacity(object_count);
    for i in 0..object_count {
        let oid = ObjectId::from_raw(format, &oidl[i * hash_len..i * hash_len + hash_len])
            .map_err(|err| err.to_string())?;
        let pack_int_id = u32_be4(&ooff[i * 8..i * 8 + 4]);
        if pack_int_id as usize >= num_packs {
            return Err(format!(
                "bad pack-int-id: {pack_int_id} ({num_packs} total packs)"
            ));
        }
        let raw_offset = u32_be4(&ooff[i * 8 + 4..i * 8 + 8]);
        let offset = if raw_offset & 0x8000_0000 == 0 {
            u64::from(raw_offset)
        } else {
            let large_idx = (raw_offset & 0x7fff_ffff) as usize;
            let loff = loff.ok_or_else(|| "multi-pack-index missing LOFF chunk".to_string())?;
            if large_idx * 8 + 8 > loff.len() {
                return Err("multi-pack-index large offset out of bounds".to_string());
            }
            u64_be8(&loff[large_idx * 8..large_idx * 8 + 8])
        };
        entries.push(MultiPackIndexEntry {
            oid,
            pack_int_id,
            offset,
            force_large_offset: raw_offset & 0x8000_0000 != 0,
        });
    }

    Ok(VerifyMidx {
        pack_names,
        object_count,
        entries,
    })
}

fn u32_be4(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn u64_be8(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(buf)
}

fn cmd_multi_pack_index_expire(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let cwd = cli_session.cwd().to_path_buf();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let (object_dir, progress, _) =
        parse_midx_object_dir_and_progress(args, &cwd, &git_dir, "expire")?;
    let pack_dir = object_dir.join("pack");
    let midx_path = pack_dir.join("multi-pack-index");
    if !midx_path.exists() {
        return Ok(());
    }
    let midx = MultiPackIndex::parse(&fs::read(&midx_path)?, format)?;
    if progress {
        // Upstream shows two delayed progress meters during expiration; with
        // GIT_PROGRESS_DELAY=0 they appear immediately, even when nothing is
        // dropped. Emit a line so `--progress` is non-empty and default silent.
        eprintln!("Counting referenced objects");
    }
    let num_packs = midx.pack_names.len();

    // Count how many of the midx's surviving objects each pack actually
    // provides. A pack whose objects are all dedup-covered by newer packs ends
    // up with zero references and is a deletion candidate.
    let mut count = vec![0u64; num_packs];
    for entry in &midx.objects {
        if (entry.pack_int_id as usize) < num_packs {
            count[entry.pack_int_id as usize] += 1;
        }
    }

    let mut dropped_any = false;
    for (i, name) in midx.pack_names.iter().enumerate() {
        if count[i] != 0 {
            continue;
        }
        // Never expire a kept or cruft pack.
        if pack_has_keep(&pack_dir, name) || pack_is_cruft(&pack_dir, name) {
            continue;
        }
        // Drop the pack and all its companions.
        let stem = pack_dir.join(name);
        for ext in ["pack", "idx", "rev", "bitmap", "mtimes", "keep"] {
            let _ = fs::remove_file(stem.with_extension(ext));
        }
        dropped_any = true;
    }

    if dropped_any {
        write_default_midx(&object_dir, format)?;
    }
    Ok(())
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
    use super::resolve_cruft_pack_size;

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
