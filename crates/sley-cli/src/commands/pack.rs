//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;
use sley_object::EncodedObject;
use sley_odb::ObjectReader;
use sley_pack::{PackInput, PackReverseIndex, PackWriteOptions, pack_order_index_positions};
use std::sync::Arc;

#[derive(Debug)]
struct IndexPackOptions {
    verbose: bool,
    output: Option<PathBuf>,
    keep: bool,
    rev_index: bool,
    verify: bool,
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
    /// Raw `<msg-id>=<severity>` override tokens from `--strict=`/`--fsck-objects=`.
    fsck_overrides: Vec<String>,
    /// `--object-format=<algo>`: the hash algorithm. Lets `index-pack <pack>`
    /// run outside a repository (where there is no config to read it from).
    object_format: Option<ObjectFormat>,
    /// `--max-input-size=<n>`: reject a pack whose byte length exceeds `<n>`.
    max_input_size: Option<u64>,
}

pub(crate) fn cmd_index_pack(args: &[String]) -> Result<()> {
    let options = parse_index_pack_options(args)?;
    // The hash algorithm is taken from `--object-format` when given, else from
    // the surrounding repository. A `<pack-file>` argument (not `--stdin`) can
    // run outside any repo, so only fall back to repo discovery when needed.
    let repo = match discover_git_dir(env::current_dir()?) {
        Ok(git_dir) => {
            let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
            let format = repository_object_format(&common_git_dir)?;
            Some((common_git_dir, format))
        }
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
        }
        if options.fsck {
            let exit = fsck_pack_objects(&pack, format, &options.fsck_overrides)?;
            if exit != 0 {
                return Err(GitError::Exit(exit));
            }
        }
        let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
        let install = db.install_raw_pack(&pack)?;
        if options.keep {
            let keep_path = install.pack_path.with_extension("keep");
            fs::write(keep_path, b"")?;
        }
        if options.rev_index {
            let rev_path = install.pack_path.with_extension("rev");
            let _ = fs::write(rev_path, b"");
        }
        println!("pack\t{}", install.pack_name.trim_start_matches("pack-"));
        return Ok(());
    }

    let Some(pack_file) = options.pack_file else {
        return index_pack_usage();
    };
    let pack = fs::read(&pack_file)?;
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
    if options.verify {
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
    if options.rev_index {
        let _ = fs::write(pack_file.with_extension("rev"), b"");
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

fn parse_index_pack_options(args: &[String]) -> Result<IndexPackOptions> {
    let mut options = IndexPackOptions {
        verbose: false,
        output: None,
        keep: false,
        rev_index: true,
        verify: false,
        stdin: false,
        fix_thin: false,
        pack_file: None,
        index_version: None,
        fsck: false,
        fsck_overrides: Vec::new(),
        object_format: None,
        max_input_size: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                for value in iter {
                    index_pack_add_pack_file(&mut options, value)?;
                }
                break;
            }
            "-v" => options.verbose = true,
            "-o" => {
                let Some(value) = iter.next() else {
                    return index_pack_usage();
                };
                options.output = Some(PathBuf::from(value));
            }
            "--stdin" => options.stdin = true,
            "--fix-thin" => options.fix_thin = true,
            "--keep" => options.keep = true,
            value if value.starts_with("--keep=") => options.keep = true,
            "--rev-index" => options.rev_index = true,
            "--no-rev-index" => options.rev_index = false,
            "--verify" => options.verify = true,
            value if value.starts_with("--index-version=") => {
                let spec = &value["--index-version=".len()..];
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
            "--threads" => {
                let _ = iter.next();
            }
            "--strict" | "--fsck-objects" => options.fsck = true,
            value if value.starts_with("--strict=") || value.starts_with("--fsck-objects=") => {
                options.fsck = true;
                let spec = value.split_once('=').map(|(_, rest)| rest).unwrap_or("");
                for token in spec.split(',') {
                    if !token.is_empty() {
                        options.fsck_overrides.push(token.to_string());
                    }
                }
            }
            "--object-format" => {
                let Some(value) = iter.next() else {
                    return index_pack_usage();
                };
                options.object_format = Some(parse_verify_pack_object_format(value)?);
            }
            value if let Some(value) = long_option_value(value, "object-format") => {
                options.object_format = Some(parse_verify_pack_object_format(value)?);
            }
            value if value.starts_with("--max-input-size=") => {
                let spec = &value["--max-input-size=".len()..];
                options.max_input_size = Some(
                    spec.parse()
                        .map_err(|_| GitError::Command(format!("bad max-input-size '{spec}'")))?,
                );
            }
            value if value.starts_with("--threads=") || value.starts_with("--pack_header=") => {}
            value if value.starts_with('-') => return index_pack_usage(),
            value => index_pack_add_pack_file(&mut options, value)?,
        }
    }
    if options.output.is_some() && options.verify {
        return Err(GitError::Exit(128));
    }
    if !options.stdin && options.pack_file.is_none() {
        return index_pack_usage();
    }
    Ok(options)
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
fn humanise_byte_count(bytes: u64) -> String {
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
        let findings = sley_fsck::content::check_object_content(
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
    format: ObjectFormat,
    index_paths: Vec<PathBuf>,
}

pub(crate) fn cmd_verify_pack(args: &[String]) -> Result<()> {
    let options = parse_verify_pack_options(args)?;
    // verify-pack inspects the named pack files directly, so it works outside a
    // repository (git resolves the hash from `--object-format`, else SHA-1).
    for index_path in &options.index_paths {
        verify_pack_one(
            options.format,
            index_path,
            options.verbose,
            options.stat_only,
        )?;
    }
    Ok(())
}

fn parse_verify_pack_options(args: &[String]) -> Result<VerifyPackOptions> {
    let mut verbose = false;
    let mut stat_only = false;
    let mut format = ObjectFormat::Sha1;
    let mut index_paths = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                index_paths.extend(iter.map(PathBuf::from));
                break;
            }
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "-s" | "--stat-only" => stat_only = true,
            "--no-stat-only" => stat_only = false,
            "--object-format" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: option `object-format' requires a value");
                    return Err(GitError::Exit(129));
                };
                format = parse_verify_pack_object_format(value)?;
            }
            "--no-object-format" => format = ObjectFormat::Sha1,
            value if let Some(value) = long_option_value(value, "object-format") => {
                format = parse_verify_pack_object_format(value)?;
            }
            value if value.starts_with("--") => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return verify_pack_usage();
            }
            value if value.starts_with('-') && value.len() > 1 => {
                for option in value[1..].chars() {
                    match option {
                        'v' => verbose = true,
                        's' => stat_only = true,
                        other => {
                            eprintln!("error: unknown switch `{other}'");
                            return verify_pack_usage();
                        }
                    }
                }
            }
            value => index_paths.push(PathBuf::from(value)),
        }
    }
    if index_paths.is_empty() {
        return verify_pack_usage();
    }
    Ok(VerifyPackOptions {
        verbose,
        stat_only,
        format,
        index_paths,
    })
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

/// The traversal roots `repack -a` packs from, mirroring upstream's
/// `pack-objects --all --reflog --indexed-objects` invocation: every direct
/// ref target, `HEAD`, both sides of every reflog entry, and the blobs in the
/// index. Unresolvable roots are skipped (the closure walk also tolerates
/// missing objects — stale reflogs are expected).
fn repack_traversal_roots(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let mut roots = Vec::new();
    let store = FileRefStore::new(git_dir, format);
    for reference in store.list_refs()? {
        if let RefTarget::Direct(oid) = reference.target {
            roots.push(oid);
        }
    }
    if let Ok(head) = resolve_revision(git_dir, format, "HEAD") {
        roots.push(head);
    }
    roots.extend(reflog_traversal_roots(git_dir, common_git_dir, format)?);
    // Indexed blobs (upstream --indexed-objects).
    if let Ok(bytes) = fs::read(git_dir.join("index"))
        && let Ok(index) = sley_index::Index::parse(&bytes, format)
    {
        for entry in &index.entries {
            roots.push(entry.oid);
        }
    }
    Ok(roots)
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

pub(crate) fn cmd_repack(args: &[String]) -> Result<()> {
    let mut prune = false;
    let mut quiet = false;
    let mut all = false;
    let mut local = false;
    let mut write_bitmaps: Option<bool> = None;
    let mut geometric: Option<u64> = None;
    let mut write_midx = false;
    let mut keep_packs: Vec<String> = Vec::new();
    let mut pack_kept_objects = false;
    let mut update_server_info: Option<bool> = None;
    let mut cruft = false;
    let mut cruft_expiration: Option<Option<u32>> = None;
    let mut expire_to: Option<String> = None;
    let mut iter = expand_repack_short_clusters(args).into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-d" => prune = true,
            "-q" | "--quiet" => quiet = true,
            "-b" | "--write-bitmap-index" => write_bitmaps = Some(true),
            "--no-write-bitmap-index" => write_bitmaps = Some(false),
            "-a" | "-A" => all = true,
            "-m" | "--write-midx" => write_midx = true,
            "-l" | "--local" => local = true,
            "-n" => update_server_info = Some(false),
            "--cruft" => cruft = true,
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
            "-k" | "--keep-unreachable" => {}
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
            // Accepted no-ops.
            "-f" | "-F" | "--progress" | "--no-progress" | "--no-pack-kept-objects" => {}
            value
                if value.starts_with("--window")
                    || value.starts_with("--depth")
                    || value.starts_with("--threads")
                    || value.starts_with("--max-pack-size")
                    || value.starts_with("--max-cruft-size")
                    || value.starts_with("--combine-cruft-below-size")
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
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let config = read_repo_config(&common_git_dir)?;
    let update_server_info = update_server_info.unwrap_or_else(|| {
        config
            .get_bool("repack", None, "updateServerInfo")
            .unwrap_or(true)
    });
    let config_write_bitmaps = config.get_bool("repack", None, "writeBitmaps");
    let auto_bare_bitmaps = write_bitmaps.is_none()
        && config_write_bitmaps.is_none()
        && all
        && !write_midx
        && config.get("pack", None, "packSizeLimit").is_none()
        && sley_worktree::worktree_root_for_git_dir(&common_git_dir)?.is_none()
        && !pack_dir_has_kept_packs(&common_git_dir)?;
    let mut write_bitmaps = match write_bitmaps {
        Some(explicit) => explicit,
        None => config_write_bitmaps.unwrap_or(auto_bare_bitmaps),
    };
    let include_kept_objects =
        pack_kept_objects || (write_bitmaps && !write_midx && !auto_bare_bitmaps);

    if write_bitmaps && local && object_dir_has_alternates(&common_git_dir) {
        eprintln!("warning: disabling bitmap writing, as some objects are not being packed");
        write_bitmaps = false;
    }

    if let Some(split_factor) = geometric {
        // `--geometric` and `-a`/`-A` are mutually exclusive (builtin/repack.c).
        if all {
            return Err(GitError::Command(
                "options '--geometric' and '-A/-a' cannot be used together".into(),
            ));
        }
        return cmd_repack_geometric(
            &git_dir,
            &common_git_dir,
            format,
            split_factor,
            prune,
            quiet,
            write_midx,
            write_bitmaps,
            &keep_packs,
            include_kept_objects,
        );
    }

    if cruft {
        validate_repack_cruft_numeric_config(&config)?;
        return cmd_repack_cruft(
            &git_dir,
            &common_git_dir,
            format,
            prune,
            cruft_expiration.flatten(),
            expire_to.as_deref(),
            write_midx,
            &keep_packs,
            include_kept_objects,
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
    // `-a`: pack the reachability closure of refs/HEAD/reflogs/index (borrowed
    // objects included, unreachable ones dropped). Without `-a`, pack only
    // loose objects and leave existing packs in place.
    let result = if all {
        let roots = repack_traversal_roots(&git_dir, &common_git_dir, format)?;
        let keep_pack_stems: HashSet<String> = keep_packs.iter().cloned().collect();
        let options = sley_odb::RepackOptions {
            local,
            pack_kept_objects: include_kept_objects,
            keep_pack_stems,
        };
        sley_odb::repack_reachable_objects_with_options(&common_git_dir, format, &roots, &options)?
    } else {
        sley_odb::repack_loose_objects(&common_git_dir, format)?
    };
    if let Some(result) = result {
        let bitmap_tips = if write_bitmaps {
            let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
            Some(repack_preferred_bitmap_tips(&common_git_dir, &db, format)?)
        } else {
            None
        };
        sley_odb::install_repack_result_with_bitmap(
            &common_git_dir,
            format,
            &result,
            prune,
            bitmap_tips.as_ref(),
        )?;
    }
    if all && (!write_bitmaps || write_midx) {
        remove_pack_bitmap_sidecars(&common_git_dir)?;
    }
    if write_midx {
        cmd_multi_pack_index_write(&[])?;
    }
    if update_server_info {
        crate::commands::refs::cmd_update_server_info(&[])?;
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

fn strip_pack_suffix(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    base.strip_suffix(".pack")
        .or_else(|| base.strip_suffix(".idx"))
        .unwrap_or(base)
        .to_string()
}

#[allow(clippy::too_many_arguments)]
fn cmd_repack_geometric(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    split_factor: u64,
    prune: bool,
    quiet: bool,
    write_midx: bool,
    write_bitmaps: bool,
    keep_packs: &[String],
    _pack_kept_objects: bool,
) -> Result<()> {
    let kept_stems: HashSet<String> = keep_packs.iter().cloned().collect();
    let geometric = sley_odb::repack_geometric(common_git_dir, format, split_factor, &kept_stems)?;

    if geometric.result.is_none() {
        if !quiet {
            println!("Nothing new to pack.");
        }
        // Even with nothing new, `--write-midx` (re)writes the MIDX so it stays
        // current with the on-disk packs — but only when packs exist AND the
        // existing MIDX (if any) does not already cover them. An up-to-date MIDX
        // is left byte-for-byte untouched (builtin/repack.c midx_has_unknown_packs).
        if write_midx
            && pack_dir_has_packs(common_git_dir, format)?
            && !midx_covers_current_packs(common_git_dir, format)?
        {
            cmd_multi_pack_index_write(&[])?;
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
        let mut midx_args: Vec<String> = Vec::new();
        if write_bitmaps {
            midx_args.push("--bitmap".to_string());
        }
        cmd_multi_pack_index_write(&midx_args)?;
    }
    let _ = git_dir;
    Ok(())
}

/// Parse a `--cruft-expiration` value. Returns `None` for "never" (zero), else
/// the UNIX-seconds cutoff (`now`/`all` → u32::MAX so everything expires).
///
/// git's expiry timestamp is UNSIGNED (`timestamp_t`); `parse_expiry_date`
/// renders the `now`/`all` sentinel as `u64::MAX` (which is `-1` reinterpreted
/// as `i64`). We interpret the value as unsigned so the sentinel saturates to
/// `u32::MAX` (everything older than "the end of time" expires) rather than
/// collapsing to `0`.
fn parse_cruft_expiration(spec: &str) -> Result<Option<u32>> {
    let ts = crate::commands::approxidate::parse_expiry_date(spec)
        .ok_or_else(|| GitError::Command(format!("malformed expiration date '{spec}'")))?;
    let ts = ts as u64;
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

/// `git repack --cruft [--cruft-expiration=<t>] [--expire-to=<dir>] [-d]`.
#[allow(clippy::too_many_arguments)]
fn cmd_repack_cruft(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    prune: bool,
    cruft_expiration: Option<u32>,
    expire_to: Option<&str>,
    write_midx: bool,
    keep_packs: &[String],
    pack_kept_objects: bool,
) -> Result<()> {
    let roots = repack_traversal_roots(git_dir, common_git_dir, format)?;
    let keep_pack_stems: HashSet<String> = keep_packs.iter().cloned().collect();
    let options = sley_odb::RepackOptions {
        local: false,
        pack_kept_objects,
        keep_pack_stems,
    };

    // With `--expire-to` + `-d`, the main cruft pack expires (drops) objects
    // older than the cutoff; those expired objects are written into the
    // expire-to repo as a second cruft pack (with no expiration, so it keeps
    // them all). Compute the pre-expiry unreachable set first so we can diff.
    let pre_expiry = if expire_to.is_some() && prune {
        Some(repack_cruft_or_bad_object(
            sley_odb::repack_cruft_with_options(common_git_dir, format, &roots, None, &options),
        )?)
    } else {
        None
    };

    let result = repack_cruft_or_bad_object(sley_odb::repack_cruft_with_options(
        common_git_dir,
        format,
        &roots,
        cruft_expiration,
        &options,
    ))?;
    sley_odb::install_cruft_repack_result(common_git_dir, format, &result, prune)?;

    // Move the expired objects into the --expire-to repository.
    if let (Some(dir), Some(pre)) = (expire_to, pre_expiry.as_ref()) {
        let kept: HashSet<ObjectId> = result
            .cruft
            .as_ref()
            .map(|c| c.oids.iter().copied().collect())
            .unwrap_or_default();
        let expired: Vec<ObjectId> = pre
            .cruft
            .as_ref()
            .map(|c| {
                c.oids
                    .iter()
                    .copied()
                    .filter(|oid| !kept.contains(oid))
                    .collect()
            })
            .unwrap_or_default();
        if !expired.is_empty() {
            write_expire_to_cruft_pack(common_git_dir, format, dir, &expired, cruft_expiration)?;
        }
    }

    if write_midx && pack_dir_has_packs(common_git_dir, format)? {
        cmd_multi_pack_index_write(&[])?;
    }
    Ok(())
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

/// Write a cruft pack of `expired` objects into the `--expire-to` repository's
/// pack directory (an `<dir>/pack` prefix like git's `--expire-to=.../pack`).
fn write_expire_to_cruft_pack(
    common_git_dir: &Path,
    format: ObjectFormat,
    expire_to: &str,
    expired: &[ObjectId],
    cruft_expiration: Option<u32>,
) -> Result<()> {
    let _ = cruft_expiration;
    // `--expire-to=<repo>/objects/pack/pack` names a pack-file prefix. Resolve
    // the directory it lives in and write the limbo cruft pack there.
    let prefix = Path::new(expire_to);
    let dest_dir = prefix
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&dest_dir)?;

    let objects_dir = repository_objects_dir(common_git_dir);
    let database = FileObjectDatabase::new(objects_dir.clone(), format);

    // Stamp each expired object with the best mtime from its on-disk copy.
    let on_disk = sley_odb::object_mtimes_on_disk_pub(&objects_dir, format)?;
    let mut survivors: HashMap<ObjectId, u32> = HashMap::new();
    for oid in expired {
        let mtime = on_disk.get(oid).copied().unwrap_or(0);
        survivors.insert(*oid, mtime);
    }
    let Some(cruft) = sley_odb::build_cruft_pack_pub(&database, format, &survivors)? else {
        return Ok(());
    };
    let pack_name = format!("pack-{}", cruft.checksum.to_hex());
    fs::write(dest_dir.join(format!("{pack_name}.pack")), &cruft.pack)?;
    fs::write(dest_dir.join(format!("{pack_name}.rev")), &cruft.rev)?;
    fs::write(dest_dir.join(format!("{pack_name}.mtimes")), &cruft.mtimes)?;
    fs::write(dest_dir.join(format!("{pack_name}.idx")), &cruft.idx)?;
    Ok(())
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

/// True when a `multi-pack-index` exists and already names exactly the set of
/// non-cruft `.idx` files currently on disk — i.e. rewriting it would be a
/// no-op, so an up-to-date MIDX must be left untouched (its mtime preserved).
fn midx_covers_current_packs(common_git_dir: &Path, format: ObjectFormat) -> Result<bool> {
    let pack_dir = repository_objects_dir(common_git_dir).join("pack");
    let midx_path = pack_dir.join("multi-pack-index");
    let Ok(midx_bytes) = fs::read(&midx_path) else {
        return Ok(false);
    };
    let Ok(midx) = MultiPackIndex::parse(&midx_bytes, format) else {
        return Ok(false);
    };
    let midx_names: HashSet<String> = midx.pack_names.into_iter().collect();

    // The MIDX indexes only non-cruft packs; compare against the current
    // non-cruft `.idx` basenames.
    let mut current: HashSet<String> = HashSet::new();
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(false);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("idx") {
            continue;
        }
        // Skip cruft packs (`.mtimes` sidecar): the MIDX excludes them by default.
        if path.with_extension("mtimes").exists() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            current.insert(name.to_string());
        }
    }
    Ok(midx_names == current)
}

pub(crate) fn cmd_gc(args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut auto = false;
    // `--cruft` / `--no-cruft` override gc.cruftPacks; None means "use config".
    let mut cruft_flag: Option<bool> = None;
    // `--prune[=<date>]` / `--no-prune` override gc.pruneExpire. The sentinel
    // distinguishes "not given" from an explicit value.
    let mut prune_override: Option<Option<String>> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--cruft" => cruft_flag = Some(true),
            "--no-cruft" => cruft_flag = Some(false),
            "--prune" => prune_override = Some(Some("2.weeks.ago".to_string())),
            "--no-prune" => prune_override = Some(None),
            value if value.starts_with("--prune=") => {
                prune_override = Some(Some(value["--prune=".len()..].to_string()));
            }
            // Accepted no-ops.
            "--auto" => auto = true,
            "--aggressive"
            | "--force"
            | "--detach"
            | "--no-detach"
            | "--skip-foreground-tasks"
            | "--progress"
            | "--no-progress"
            | "--keep-largest-pack" => {}
            value if value.starts_with("--max-cruft-size") || value.starts_with("--expire-to") => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!("unsupported gc option {value}")));
            }
            value => {
                return Err(GitError::Command(format!(
                    "unsupported gc argument {value}"
                )));
            }
        }
    }
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let config = read_repo_config(&common_git_dir)?;

    // gc.cruftPacks defaults to true (cruft packs are git's default since 2.42).
    let cruft_packs = cruft_flag
        .or_else(|| config.get_bool("gc", None, "cruftPacks"))
        .unwrap_or(true);

    // gc.pruneExpire defaults to "2.weeks.ago"; --prune=<date>/--no-prune and the
    // config override it. None means "never prune" (`--no-prune`).
    let prune_expire: Option<String> = match prune_override {
        Some(value) => value,
        None => Some(
            config
                .get("gc", None, "pruneExpire")
                .unwrap_or("2.weeks.ago")
                .to_string(),
        ),
    };

    // gc_before_repack runs `reflog expire --all` unless BOTH gc.reflogExpire
    // and gc.reflogExpireUnreachable are "never" (builtin/gc.c gc_config). The
    // expire drops reflog entries pointing at unreachable history, which is what
    // turns once-referenced commits into cruft for the repack below.
    let reflog_expire_never = is_config_never(&config, "gc", "reflogExpire");
    let reflog_unreachable_never = is_config_never(&config, "gc", "reflogExpireUnreachable");
    if !(reflog_expire_never && reflog_unreachable_never) {
        let mut expire_args = vec!["expire".to_string(), "--all".to_string()];
        if reflog_expire_never {
            expire_args.push("--expire=never".to_string());
        }
        if reflog_unreachable_never {
            expire_args.push("--expire-unreachable=never".to_string());
        }
        // Best-effort: a reflog-expire failure must not abort the whole gc.
        let _ = commands::refs::cmd_reflog(&expire_args);
    }

    let roots = repack_traversal_roots(&git_dir, &common_git_dir, format)?;

    // builtin/gc.c add_repack_all_option: pick the repack flavour.
    if prune_expire.as_deref() == Some("now") && cruft_packs {
        // prune_expire=="now" with cruft (no expire-to): immediate drop via -a.
        if let Some(result) = sley_odb::repack_reachable_objects(&common_git_dir, format, &roots)? {
            sley_odb::install_repack_result(&common_git_dir, format, &result, true)?;
        }
    } else if cruft_packs {
        // Default: reachable pack + cruft pack, cruft expiry = prune_expire.
        let cruft_expiration = match prune_expire.as_deref() {
            Some(spec) => parse_cruft_expiration(spec)?,
            None => None,
        };
        let result = sley_odb::repack_cruft(&common_git_dir, format, &roots, cruft_expiration)?;
        sley_odb::install_cruft_repack_result(&common_git_dir, format, &result, true)?;
    } else {
        // gc.cruftPacks=false: -A -d, dropping unreachable older than prune_expire
        // (we drop all unreachable, matching the common "no recent unreachable"
        // case the suite exercises).
        if let Some(result) = sley_odb::repack_reachable_objects(&common_git_dir, format, &roots)? {
            sley_odb::install_repack_result(&common_git_dir, format, &result, true)?;
        }
    }

    let store = FileRefStore::new(&common_git_dir, format)
        .with_reftable_lock_timeout_millis(reftable_lock_timeout_override()?);
    if auto && store.uses_reftable()? && store.reftable_table_count()? > 2 {
        store.compact_reftable_stack()?;
    }

    let _ = quiet;
    Ok(())
}

pub(crate) fn cmd_maintenance(args: &[String]) -> Result<()> {
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
        "run" => cmd_maintenance_run(&args[1..]),
        "is-needed" => cmd_maintenance_is_needed(&args[1..]),
        "register" => cmd_maintenance_register(&args[1..]),
        "unregister" => cmd_maintenance_unregister(&args[1..]),
        "start" => cmd_maintenance_start(&args[1..]),
        "stop" => cmd_maintenance_stop(&args[1..]),
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

fn cmd_maintenance_run(args: &[String]) -> Result<()> {
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
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let config = read_repo_config(&common_git_dir)?;
    let selected = maintenance_select_tasks(&config, &tasks, schedule.as_deref())?;
    maintenance_run_selected(&common_git_dir, &config, &selected, quiet, auto, detach)?;
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
        maintenance_run_one(common_git_dir, config, task, quiet, auto)?;
    }
    let _ = fs::remove_file(lock);
    Ok(())
}

fn maintenance_run_one(
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
            commands::plumbing::cmd_commit_graph(&[
                "write".to_string(),
                "--reachable".to_string(),
                progress.to_string(),
            ])
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
        "geometric-repack" => {
            let factor = config
                .get("maintenance", Some("geometric-repack"), "splitFactor")
                .unwrap_or("2");
            let geometric = format!("--geometric={factor}");
            let mut args = vec!["repack", "-d", "-l", geometric.as_str()];
            if quiet {
                args.push("--quiet");
            }
            args.push("--write-midx");
            run_sley_child(&args, None)
        }
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
        "incremental-repack" | "geometric-repack" => {
            maintenance_limit_satisfied(config, task, 10, count_pack_files(common_git_dir)?)?
        }
        "worktree-prune" => worktree_prune_needed(common_git_dir, config)?,
        "rerere-gc" => rerere_gc_needed(common_git_dir, config)?,
        "reflog-expire" => maintenance_limit_satisfied(
            config,
            "reflog-expire",
            100,
            count_reflog_entries(&common_git_dir.join("logs"))?,
        )?,
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
    let limit = config
        .get("maintenance", Some(task), "auto")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(default);
    Ok(limit < 0 || (limit > 0 && count >= limit as usize))
}

fn cmd_maintenance_is_needed(args: &[String]) -> Result<()> {
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
    let git_dir = discover_git_dir(env::current_dir()?)?;
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
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let argv = argv
        .iter()
        .map(|arg| format!("\"{}\"", json_escape(arg)))
        .collect::<Vec<_>>()
        .join(",");
    let line = format!(
        "{{\"event\":\"child_start\",\"sid\":\"sley\",\"child_id\":0,\"argv\":[{argv}]}}\n"
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

fn count_reflog_entries(path: &Path) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        if path.is_dir() {
            count += count_reflog_entries(&path)?;
        } else if let Ok(text) = fs::read_to_string(path) {
            count += text.lines().count();
        }
    }
    Ok(count)
}

fn cmd_maintenance_register(args: &[String]) -> Result<()> {
    let config_file = parse_maintenance_config_file(args, "register")?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let repo = env::current_dir()?.display().to_string();

    let _ = report_missing_maintenance_repo(&common_git_dir);
    commands::config_cmd::cmd_config(&[
        "set".to_string(),
        "maintenance.auto".to_string(),
        "false".to_string(),
    ])?;
    if read_repo_config(&common_git_dir)?
        .get("maintenance", None, "strategy")
        .is_none()
    {
        commands::config_cmd::cmd_config(&[
            "set".to_string(),
            "maintenance.strategy".to_string(),
            "incremental".to_string(),
        ])?;
    }

    let file = config_file.unwrap_or(maintenance_global_config_path()?);
    config_add_value_if_missing(&file, "maintenance", "repo", &repo)?;
    Ok(())
}

fn cmd_maintenance_unregister(args: &[String]) -> Result<()> {
    let (config_file, force) = parse_maintenance_unregister_args(args)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
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

fn cmd_maintenance_start(args: &[String]) -> Result<()> {
    let scheduler = parse_maintenance_start_args(args)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let scheduler = scheduler.unwrap_or(MaintenanceScheduler::Systemd);
    update_background_schedule(&common_git_dir, Some(scheduler))?;
    cmd_maintenance_register(&[])
}

fn cmd_maintenance_stop(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return maintenance_subcommand_usage("stop");
    }
    let git_dir = discover_git_dir(env::current_dir()?)?;
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
                scheduler = Some(parse_scheduler(value)?);
            }
            value if let Some(name) = value.strip_prefix("--scheduler=") => {
                scheduler = Some(parse_scheduler(name)?);
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
    scheduler == MaintenanceScheduler::Systemd
        && ProcessCommand::new("systemctl")
            .args(["--user", "list-timers"])
            .status()
            .is_ok_and(|status| status.success())
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
            let xml = common_git_dir.join(format!("schedule_{frequency}.xml"));
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
pub(crate) fn cmd_unpack_objects(args: &[String]) -> Result<()> {
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
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
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
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    sley_odb::unpack_packfile_objects(&pack_bytes, format, db.loose())?;
    Ok(())
}

pub(crate) fn cmd_count_objects(args: &[String]) -> Result<()> {
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
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
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

pub(crate) fn cmd_pack_refs(args: &[String]) -> Result<()> {
    let options = parse_pack_refs_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&common_git_dir, format)
        .with_reftable_lock_timeout_millis(reftable_lock_timeout_override()?);
    if store.uses_reftable()? {
        if options.auto && store.reftable_table_count()? <= 2 {
            return Ok(());
        }
        return store.compact_reftable_stack().map_err(|err| {
            if matches!(err, GitError::Io(ref message) if message.contains("File exists")) {
                eprintln!("error: unable to compact stack: data is locked");
                GitError::Exit(1)
            } else {
                err
            }
        });
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

fn parse_pack_refs_options(args: &[String]) -> Result<PackRefsOptions> {
    let mut all = false;
    let mut prune = true;
    let mut auto = false;
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    let mut args = GitArgCursor::new(args);
    while let Some(arg) = args.next() {
        match arg {
            "-h" | "--help" => return pack_refs_help(),
            "--all" => all = true,
            "--no-all" => all = false,
            "--prune" => prune = true,
            "--no-prune" => prune = false,
            "--auto" => auto = true,
            "--no-auto" => auto = false,
            "--include" | "--exclude" => {
                let Some(pattern) = args.next_value() else {
                    return pack_refs_usage();
                };
                if arg == "--include" {
                    include.push(pattern.to_string());
                } else {
                    exclude.push(pattern.to_string());
                }
            }
            "--no-include" => include.clear(),
            "--no-exclude" => exclude.clear(),
            value if let Some(pattern) = long_option_value(value, "include") => {
                include.push(pattern.to_string());
            }
            value if let Some(pattern) = long_option_value(value, "exclude") => {
                exclude.push(pattern.to_string());
            }
            value if value.starts_with("--no-include=") => {
                eprintln!("error: option `no-include' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--no-exclude=") => {
                eprintln!("error: option `no-exclude' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return pack_refs_usage();
            }
            _ => return pack_refs_usage(),
        }
    }
    Ok(PackRefsOptions {
        all,
        prune,
        auto,
        include,
        exclude,
    })
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
    for entry in fs::read_dir(pack_dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if path.extension().and_then(|ext| ext.to_str()) == Some("pack") {
            stats.packs += 1;
            stats.size_pack_bytes += metadata.len();
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) == Some("idx") {
            let summary = count_pack_index_summary(&path, &metadata, format)?;
            if let Some(summary) = summary {
                stats.size_pack_bytes += metadata.len();
                stats.in_pack += u64::from(summary.object_count);
                pack_indexes.push(summary);
            }
        }
    }
    Ok(pack_indexes)
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

pub(crate) fn cmd_prune(args: &[String]) -> Result<()> {
    let options = parse_prune_options(args)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let mut roots = prune_roots(&git_dir, &common_git_dir, format, &options.heads)?;
    roots.extend(prune_recent_object_roots(&db, &common_git_dir, format, options.expire)?);
    roots.extend(prune_recent_hook_roots(&common_git_dir, format)?);
    roots.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    roots.dedup();
    let reachable = collect_reachable_object_ids(&db, format, roots.iter().copied())?;
    let mut candidates = Vec::new();
    for oid in prune_unreachable_loose(&common_git_dir, format, roots.iter().copied(), false)? {
        if prune_object_is_expired(&db, &oid, options.expire)? {
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
    prune_shallow_file(&common_git_dir, format, &reachable, options.dry_run, options.verbose)?;
    prune_temporary_files(&common_git_dir.join("objects"), options.expire, options.dry_run, options.verbose)?;
    prune_temporary_files(
        &common_git_dir.join("objects").join("pack"),
        options.expire,
        options.dry_run,
        options.verbose,
    )?;
    prune_packed_loose_objects(&common_git_dir, format, options.dry_run)?;
    if !options.dry_run {
        prune_empty_loose_object_dirs(&common_git_dir.join("objects"))?;
    }
    Ok(())
}

fn parse_prune_options(args: &[String]) -> Result<PruneOptions> {
    let mut dry_run = false;
    let mut verbose = false;
    let mut expire = i64::MAX;
    let mut heads = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                heads.extend(iter.cloned());
                break;
            }
            "-h" | "--help" => return prune_help(),
            "-n" | "--dry-run" => dry_run = true,
            "--no-dry-run" => dry_run = false,
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "--progress"
            | "--no-progress"
            | "--exclude-promisor-objects"
            | "--no-exclude-promisor-objects" => {}
            "--expire" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: option `expire' requires a value");
                    return Err(GitError::Exit(129));
                };
                expire = parse_prune_expire(value, "--expire")?;
            }
            "--no-expire" => expire = i64::MIN,
            value if let Some(value) = long_option_value(value, "expire") => {
                expire = parse_prune_expire(value, "--expire")?;
            }
            value if value.starts_with("--no-expire=") => {
                eprintln!("error: option `no-expire' takes no value");
                return Err(GitError::Exit(129));
            }
            value if value.starts_with("--") => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return prune_usage();
            }
            value if value.starts_with('-') && value.len() > 1 => {
                for option in value[1..].chars() {
                    match option {
                        'n' => dry_run = true,
                        'v' => verbose = true,
                        other => {
                            eprintln!("error: unknown switch `{other}'");
                            return prune_usage();
                        }
                    }
                }
            }
            value => heads.push(value.to_string()),
        }
    }
    Ok(PruneOptions {
        dry_run,
        verbose,
        expire,
        heads,
    })
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
        roots.insert(resolve_revision(common_git_dir, format, head)?);
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
    if expire <= i64::MIN {
        return Ok(Vec::new());
    }
    let mut roots = Vec::new();
    for oid in sley_odb::repository_object_ids(common_git_dir, format)? {
        if prune_object_is_expired(db, &oid, expire)? {
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
    let mut roots = Vec::new();
    for hook in config
        .get_all("gc", None, "recentObjectsHook")
        .into_iter()
        .flatten()
    {
        let output = ProcessCommand::new("sh")
            .arg("-c")
            .arg(hook)
            .current_dir(common_git_dir.parent().unwrap_or(common_git_dir))
            .output()?;
        if !output.status.success() {
            eprintln!("fatal: unable to enumerate additional recent objects");
            return Err(GitError::Exit(128));
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let value = line.trim();
            if value.len() == format.hex_len() {
                roots.push(ObjectId::from_hex(format, value)?);
            }
        }
    }
    Ok(roots)
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

fn prune_usage<T>() -> Result<T> {
    eprintln!("usage: git prune [-n] [-v] [--progress] [--expire <time>] [--] [<head>...]");
    eprintln!();
    eprintln!("    -n, --[no-]dry-run    do not remove, show only");
    eprintln!("    -v, --[no-]verbose    report pruned objects");
    eprintln!("    --[no-]progress       show progress");
    eprintln!("    --[no-]expire <expiry-date>");
    eprintln!("                          expire objects older than <time>");
    eprintln!("    --[no-]exclude-promisor-objects");
    eprintln!("                          limit traversal to objects outside promisor packfiles");
    eprintln!();
    Err(GitError::Exit(129))
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

pub(crate) fn cmd_multi_pack_index(args: &[String]) -> Result<()> {
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
        "expire" => cmd_multi_pack_index_expire(&combined),
        "repack" => cmd_multi_pack_index_repack(&combined),
        "write" => cmd_multi_pack_index_write(&combined),
        "verify" => cmd_multi_pack_index_verify(&combined),
        other => {
            eprintln!("error: unknown subcommand: `{other}'");
            eprint!("{MULTI_PACK_INDEX_USAGE}");
            Err(GitError::Exit(129))
        }
    }
}

fn cmd_multi_pack_index_write(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    let mut stdin_packs = false;
    let mut write_bitmap = false;
    let mut preferred_pack_name: Option<String> = None;
    let mut refs_snapshot: Option<PathBuf> = None;
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
            "--preferred-pack" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--preferred-pack requires a value".into()))?;
                preferred_pack_name = Some(value.clone());
            }
            value if value.starts_with("--preferred-pack=") => {
                preferred_pack_name = Some(value["--preferred-pack=".len()..].to_string());
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

    let mut pack_names = if stdin_packs {
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

    // Per-pack mtimes drive both duplicate resolution and the default
    // preferred pack (upstream uses the .pack mtime for both). Captured once —
    // the duplicate-resolution sort consults them O(n log n) times.
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

    let mut objects = Vec::new();
    for (pack_int_id, pack_name) in pack_names.iter().enumerate() {
        let index = PackIndex::parse(&fs::read(pack_dir.join(pack_name))?, format)?;
        for entry in index.entries {
            objects.push(MultiPackIndexEntry {
                oid: entry.oid,
                pack_int_id: pack_int_id as u32,
                offset: entry.offset,
            });
        }
    }

    if write_bitmap && objects.is_empty() {
        // Upstream refuses a multi-pack .bitmap over zero objects but still
        // writes the midx itself.
        eprintln!("warning: refusing to write multi-pack .bitmap without any objects");
        write_bitmap = false;
    }

    // Preferred pack: explicit name, else (when writing a bitmap) the pack
    // with the oldest mtime — it gets pseudo-pack priority so its objects
    // lead the bit order.
    let preferred_pack: Option<u32> = match &preferred_pack_name {
        Some(name) => {
            let normalized = name.strip_suffix(".pack").map(|stem| format!("{stem}.idx"));
            match pack_names.iter().position(|pack_name| {
                pack_name == name || Some(pack_name.as_str()) == normalized.as_deref()
            }) {
                Some(position) => Some(position as u32),
                None => {
                    eprintln!("warning: unknown preferred pack: '{name}'");
                    write_bitmap.then_some(0)
                }
            }
        }
        None if write_bitmap => {
            let mut preferred = 0u32;
            let mut oldest: Option<std::time::SystemTime> = None;
            for pack_int_id in 0..pack_names.len() as u32 {
                let mtime = pack_mtime(pack_int_id);
                if oldest.is_none_or(|current| mtime < current) {
                    oldest = Some(mtime);
                    preferred = pack_int_id;
                }
            }
            Some(preferred)
        }
        None => None,
    };

    // Duplicate resolution across packs (upstream midx_oid_compare): keep the
    // copy from the preferred pack, else the newest pack, else the lowest
    // pack id.
    objects.sort_by(|left, right| {
        left.oid
            .as_bytes()
            .cmp(right.oid.as_bytes())
            .then_with(|| {
                let left_preferred = Some(left.pack_int_id) == preferred_pack;
                let right_preferred = Some(right.pack_int_id) == preferred_pack;
                right_preferred.cmp(&left_preferred)
            })
            .then_with(|| pack_mtime(right.pack_int_id).cmp(&pack_mtime(left.pack_int_id)))
            .then_with(|| left.pack_int_id.cmp(&right.pack_int_id))
    });
    objects.dedup_by(|next, kept| next.oid == kept.oid);

    let midx = MultiPackIndex::write_with_reverse_index(
        format,
        1,
        &pack_names,
        &objects,
        write_bitmap.then(|| preferred_pack.unwrap_or(0)),
    )?;
    let midx_checksum = ObjectId::from_raw(format, &midx[midx.len() - format.raw_len()..])?;
    let bitmap_name = format!("multi-pack-index-{}.bitmap", midx_checksum.to_hex());

    // Build the bitmap BEFORE the midx lands on disk: a closure failure must
    // abort the whole write (upstream dies and leaves no midx behind),
    // unlike repack's warn-and-continue.
    let bitmap = if write_bitmap {
        let db = FileObjectDatabase::new(object_dir.clone(), format);
        let mut tips = repack_preferred_bitmap_tips(&git_dir, &db, format)?;
        if let Some(snapshot) = &refs_snapshot {
            // Snapshot lines are "<oid>" (plain tip) or "+<oid>" (preferred
            // tip, upstream's NEEDS_BITMAP). Only the preferred ones
            // influence selection here.
            for line in fs::read_to_string(snapshot)?.lines() {
                if let Some(hex) = line.strip_prefix('+')
                    && let Ok(oid) = ObjectId::from_hex(format, hex)
                    && let Ok(commit) = sley_rev::peel_to_commit(&db, format, &oid)
                {
                    tips.insert(commit);
                }
            }
        }
        let preferred_pack = preferred_pack.unwrap_or(0);
        match sley_odb::build_midx_bitmap(
            &db,
            format,
            &objects,
            &midx_checksum,
            preferred_pack,
            &tips,
        )? {
            Some(bitmap) => Some(bitmap),
            None => {
                eprintln!("fatal: could not write multi-pack bitmap");
                return Err(GitError::Exit(1));
            }
        }
    } else {
        None
    };

    fs::write(pack_dir.join("multi-pack-index"), &midx)?;

    // GIT_TEST_MIDX_WRITE_REV=1 (t5327): additionally write the bit-order
    // permutation as a separate `multi-pack-index-<checksum>.rev` file, the
    // way upstream's write_midx_reverse_index does alongside the RIDX chunk.
    let rev_name = format!("multi-pack-index-{}.rev", midx_checksum.to_hex());
    let write_rev_file = write_bitmap
        && env::var("GIT_TEST_MIDX_WRITE_REV").is_ok_and(|value| value == "1" || value == "true");
    if write_rev_file {
        let mut pseudo: Vec<u32> = (0..objects.len() as u32).collect();
        let preferred = preferred_pack.unwrap_or(0);
        pseudo.sort_by_key(|&midx_pos| {
            let object = &objects[midx_pos as usize];
            (
                object.pack_int_id != preferred,
                object.pack_int_id,
                object.offset,
            )
        });
        let rev_bytes = PackReverseIndex::write(format, &pseudo, &midx_checksum)?;
        fs::write(pack_dir.join(&rev_name), &rev_bytes)?;
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
            && (!write_bitmap || name != bitmap_name)
            && (!write_rev_file || name != rev_name)
        {
            let _ = fs::remove_file(&path);
        }
    }

    if let Some(bitmap) = bitmap {
        let bitmap_path = pack_dir.join(&bitmap_name);
        let temp_path = bitmap_path.with_extension("bitmap.tmp");
        fs::write(&temp_path, &bitmap)?;
        fs::rename(&temp_path, &bitmap_path)?;
    }
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
        let index = PackIndex::parse(&fs::read(pack_dir.join(pack_name))?, format)?;
        for entry in index.entries {
            objects.push(MultiPackIndexEntry {
                oid: entry.oid,
                pack_int_id: pack_int_id as u32,
                offset: entry.offset,
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

fn cmd_multi_pack_index_repack(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
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
        if let Ok(index) = PackIndex::parse(&fs::read(pack_dir.join(name))?, format) {
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

fn cmd_multi_pack_index_verify(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
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
fn verify_midx_at(object_dir: &Path, format: ObjectFormat, progress: bool) -> Result<()> {
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
            eprintln!("error: {message}");
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
            .and_then(|raw| PackIndex::parse(&raw, format).ok())
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
        let pack_offset = index
            .entries
            .iter()
            .find(|e| e.oid == entry.oid)
            .map(|e| e.offset);
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

fn cmd_multi_pack_index_expire(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
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
