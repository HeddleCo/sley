//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;
use sley_pack::PackReverseIndex;

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
}

pub(crate) fn cmd_index_pack(args: &[String]) -> Result<()> {
    let options = parse_index_pack_options(args)?;
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    if options.stdin {
        let mut pack = Vec::new();
        io::stdin().read_to_end(&mut pack)?;
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
    let indexed = PackFile::index_pack(&pack, format)?;
    if options.verify {
        return Ok(());
    }
    let index_path = options
        .output
        .unwrap_or_else(|| pack_file.with_extension("idx"));
    write_index_pack_output(&index_path, &indexed.index)?;
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
            value if value.starts_with("--strict") || value.starts_with("--fsck-objects") => {}
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
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, options.format);
    for index_path in &options.index_paths {
        verify_pack_one(
            &db,
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
    db: &FileObjectDatabase,
    format: ObjectFormat,
    index_path: &Path,
    verbose: bool,
    stat_only: bool,
) -> Result<()> {
    // Upstream verify-pack accepts "foo.pack", "foo.idx" and "foo", and
    // normalizes them all to the pack/idx pair (builtin/verify-pack.c's
    // verify_one_pack). Derive the .idx path the same way.
    let index_path = {
        let path = index_path.to_string_lossy();
        let base = path
            .strip_suffix(".idx")
            .or_else(|| path.strip_suffix(".pack"))
            .unwrap_or(&path);
        PathBuf::from(format!("{base}.idx"))
    };
    let index = PackIndex::parse(&fs::read(&index_path)?, format)?;
    let mut entries = index.entries;
    entries.sort_by_key(|entry| entry.offset);
    let mut non_delta = 0usize;
    for entry in &entries {
        let Some((object_type, size)) = db.read_object_header(&entry.oid)? else {
            eprintln!("fatal: cannot read object {}", entry.oid);
            return Err(GitError::Exit(1));
        };
        let Some(storage) = db.object_storage_info(&entry.oid)? else {
            eprintln!("fatal: cannot locate object {}", entry.oid);
            return Err(GitError::Exit(1));
        };
        if storage.deltabase == ObjectId::null(format) {
            non_delta += 1;
        }
        if verbose && !stat_only {
            println!(
                "{} {:<6} {} {} {}",
                entry.oid,
                object_type.as_str(),
                size,
                storage.disk_size,
                entry.offset
            );
        }
    }
    if verbose || stat_only {
        println!("non delta: {non_delta} objects");
        if verbose && !stat_only {
            println!("{}: ok", index_path.with_extension("pack").display());
        }
    }
    Ok(())
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
            && bytes[1..]
                .iter()
                .all(|&ch| matches!(ch, b'a' | b'A' | b'b' | b'd' | b'f' | b'F' | b'l' | b'q'))
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
        if !prefixes.iter().any(|prefix| reference.name.starts_with(prefix)) {
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
    // Reflogs: parse the leading "<old> <new>" oid pair of each line under
    // logs/ (both sides — upstream's --reflog marks both).
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

pub(crate) fn cmd_repack(args: &[String]) -> Result<()> {
    let mut prune = false;
    let mut quiet = false;
    let mut all = false;
    let mut write_bitmaps: Option<bool> = None;
    for arg in &expand_repack_short_clusters(args) {
        match arg.as_str() {
            "-d" => prune = true,
            "-q" | "--quiet" => quiet = true,
            "-b" | "--write-bitmap-index" => write_bitmaps = Some(true),
            "--no-write-bitmap-index" => write_bitmaps = Some(false),
            "-a" | "-A" => all = true,
            // Accepted no-ops.
            "-l" | "-f" | "-F" | "--progress" | "--no-progress" => {}
            value if value.starts_with("--window") || value.starts_with("--depth") => {}
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
    let write_bitmaps = match write_bitmaps {
        Some(explicit) => explicit,
        None => read_repo_config(&common_git_dir)?
            .get_bool("repack", None, "writeBitmaps")
            .unwrap_or(false),
    };
    if write_bitmaps && !all {
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
        sley_odb::repack_reachable_objects(&common_git_dir, format, &roots)?
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
    let _ = quiet;
    Ok(())
}

pub(crate) fn cmd_gc(args: &[String]) -> Result<()> {
    let mut quiet = false;
    for arg in args {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            // Accepted no-ops for the M1 subset (we consolidate packs + drop
            // redundant ones; aggressive unreachable pruning is deferred).
            "--auto"
            | "--aggressive"
            | "--force"
            | "--no-detach"
            | "--prune"
            | "--no-prune"
            | "--progress"
            | "--no-progress"
            | "--keep-largest-pack" => {}
            value if value.starts_with("--prune=") => {}
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
    if let Some(result) = sley_odb::repack_all_objects(&common_git_dir, format)? {
        // gc removes packs/loose made redundant by the new pack (safe: only
        // objects already present in the new pack are dropped).
        sley_odb::install_repack_result(&common_git_dir, format, &result, true)?;
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
        "run" => cmd_maintenance_run(&args[1..]),
        _ => {
            eprintln!("error: unknown subcommand: `{subcommand}`");
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

fn cmd_maintenance_run(args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut tasks = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => {}
            "--auto" | "--no-auto" | "--detach" | "--no-detach" => {}
            "--schedule" => {
                index += 1;
                if args.get(index).is_none() {
                    eprintln!("error: option `schedule' requires a value");
                    return Err(GitError::Exit(129));
                }
            }
            value if value.starts_with("--schedule=") => {}
            "--task" => {
                index += 1;
                let Some(task) = args.get(index) else {
                    eprintln!("error: option `task' requires a value");
                    return Err(GitError::Exit(129));
                };
                tasks.push(task.clone());
            }
            value if let Some(task) = value.strip_prefix("--task=") => {
                tasks.push(task.to_string());
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                return maintenance_run_usage();
            }
            _ => return maintenance_run_usage(),
        }
        index += 1;
    }

    let run_gc = if tasks.is_empty() {
        true
    } else {
        let mut saw_gc = false;
        for task in &tasks {
            match task.as_str() {
                "gc" | "all" => saw_gc = true,
                other => {
                    eprintln!("error: '{other}' is not a valid task");
                    return Err(GitError::Exit(129));
                }
            }
        }
        saw_gc
    };

    if run_gc {
        let mut gc_args = Vec::new();
        if quiet {
            gc_args.push("--quiet".to_string());
        }
        cmd_gc(&gc_args)?;
    }
    Ok(())
}

/// `git unpack-objects` — explode a pack stream from stdin into loose objects
/// (upstream `builtin/unpack-objects.c`). `-n` parses without writing; the
/// other upstream flags are accepted and inert for this in-process path.
pub(crate) fn cmd_unpack_objects(args: &[String]) -> Result<()> {
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            "-n" => dry_run = true,
            "-q" | "-r" | "--strict" => {}
            value if value.starts_with("--pack_header=") || value.starts_with("--max-input-size=") => {
            }
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
    include: Vec<String>,
    exclude: Vec<String>,
}

pub(crate) fn cmd_pack_refs(args: &[String]) -> Result<()> {
    let options = parse_pack_refs_options(args)?;
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let store = FileRefStore::new(&common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);

    let mut packed = BTreeMap::new();
    let packed_path = common_git_dir.join("packed-refs");
    if packed_path.exists() {
        for reference in parse_packed_refs(format, &fs::read(&packed_path)?)? {
            packed.insert(reference.reference.name.clone(), reference);
        }
    }

    let mut packed_loose_names = Vec::new();
    for reference in store.list_refs()? {
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        if !pack_refs_should_include(&reference.name, &options) {
            continue;
        }
        let peeled = pack_refs_peeled_oid(&db, format, &oid)?;
        packed_loose_names.push(reference.name.clone());
        packed.insert(
            reference.name.clone(),
            PackedRef {
                reference: Ref {
                    name: reference.name,
                    target: RefTarget::Direct(oid),
                },
                peeled,
            },
        );
    }

    let refs = packed.into_values().collect::<Vec<_>>();
    store.write_packed_refs(&refs)?;
    if options.prune {
        for name in packed_loose_names {
            let _ = fs::remove_file(common_git_dir.join(&name));
        }
    }
    Ok(())
}

fn parse_pack_refs_options(args: &[String]) -> Result<PackRefsOptions> {
    let mut all = false;
    let mut prune = true;
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    let mut args = GitArgCursor::new(args);
    while let Some(arg) = args.next() {
        match arg {
            "--all" => all = true,
            "--no-all" => all = false,
            "--prune" => prune = true,
            "--no-prune" => prune = false,
            "--auto" | "--no-auto" => {}
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
    let mut packed_oids = HashSet::new();
    count_pack_objects(
        &objects_dir.join("pack"),
        format,
        &mut stats,
        &mut packed_oids,
    )?;
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
                &packed_oids,
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
    packed_oids: &HashSet<ObjectId>,
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
            if packed_oids.contains(&oid) {
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
    packed_oids: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !pack_dir.exists() {
        return Ok(());
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
        if path.extension().and_then(|ext| ext.to_str()) == Some("idx")
            && let Ok(index) = PackIndex::parse(&fs::read(path)?, format)
        {
            stats.size_pack_bytes += metadata.len();
            stats.in_pack += index.entries.len() as u64;
            packed_oids.extend(index.entries.into_iter().map(|entry| entry.oid));
        }
    }
    Ok(())
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
    let roots = prune_roots(&common_git_dir, format, &options.heads)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let mut candidates = Vec::new();
    for oid in prune_unreachable_loose(&common_git_dir, format, roots, false)? {
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
    Ok(())
}

fn parse_prune_options(args: &[String]) -> Result<PruneOptions> {
    let mut dry_run = false;
    let mut verbose = false;
    let mut expire = current_unix_seconds();
    let mut heads = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                heads.extend(iter.cloned());
                break;
            }
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
        _ => parse_reflog_expire_time(value, option),
    }
}

fn prune_roots(git_dir: &Path, format: ObjectFormat, heads: &[String]) -> Result<Vec<ObjectId>> {
    let store = FileRefStore::new(git_dir, format);
    let mut roots = BTreeSet::new();
    if let Some(oid) = resolve_ref_to_oid(&store, "HEAD")? {
        roots.insert(oid);
    }
    for reference in store.list_refs()? {
        if let Some(oid) = resolve_ref_to_oid(&store, &reference.name)? {
            roots.insert(oid);
        }
    }
    for head in heads {
        roots.insert(resolve_revision(git_dir, format, head)?);
    }
    Ok(roots.into_iter().collect())
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

pub(crate) fn cmd_multi_pack_index(args: &[String]) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        return Err(GitError::Command(
            "multi-pack-index requires <expire|write|verify>".into(),
        ));
    };
    match subcommand {
        "expire" => cmd_multi_pack_index_expire(&args[1..]),
        "write" => cmd_multi_pack_index_write(&args[1..]),
        "verify" => cmd_multi_pack_index_verify(&args[1..]),
        other => Err(GitError::Command(format!(
            "unsupported multi-pack-index subcommand {other}"
        ))),
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
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            "--progress" | "--no-progress" => {}
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
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--preferred-pack requires a value".into())
                })?;
                preferred_pack_name = Some(value.clone());
            }
            value if value.starts_with("--preferred-pack=") => {
                preferred_pack_name = Some(value["--preferred-pack=".len()..].to_string());
            }
            "--refs-snapshot" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--refs-snapshot requires a value".into())
                })?;
                refs_snapshot = Some(resolve_cli_path(&cwd, value));
            }
            value if value.starts_with("--refs-snapshot=") => {
                refs_snapshot = Some(resolve_cli_path(
                    &cwd,
                    &value["--refs-snapshot=".len()..],
                ));
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

fn cmd_multi_pack_index_verify(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            "--progress" | "--no-progress" => {}
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
    let midx_path = object_dir.join("pack").join("multi-pack-index");
    MultiPackIndex::parse(&fs::read(midx_path)?, format)?;
    Ok(())
}

fn cmd_multi_pack_index_expire(args: &[String]) -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            "--progress" | "--no-progress" => {}
            value if value.starts_with("--object-dir=") => {
                let value = value
                    .strip_prefix("--object-dir=")
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(&cwd, value));
            }
            other => {
                return Err(GitError::Unsupported(format!(
                    "multi-pack-index expire option {other}"
                )));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(&git_dir));
    let midx_path = object_dir.join("pack").join("multi-pack-index");
    if midx_path.exists() {
        MultiPackIndex::parse(&fs::read(midx_path)?, format)?;
    }
    Ok(())
}
