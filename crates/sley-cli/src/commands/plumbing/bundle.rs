//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use crate::*;

pub(crate) fn cmd_bundle(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        print_bundle_usage();
        return Err(GitError::Exit(129));
    };
    match subcommand {
        "create" => cmd_bundle_create(cli_session, &args[1..]),
        "verify" => cmd_bundle_verify(cli_session, &args[1..]),
        "list-heads" => cmd_bundle_list_heads(&args[1..]),
        "unbundle" => cmd_bundle_unbundle(cli_session, &args[1..]),
        _ => {
            print_bundle_usage();
            Err(GitError::Exit(129))
        }
    }
}

const BUNDLE_CREATE_USAGE: &str = "usage: git bundle create [-q | --quiet | --progress]\n                  [--version=<version>] <file> <git-rev-list-args>\n";
const BUNDLE_VERIFY_USAGE: &str = "usage: git bundle verify [-q | --quiet] <file>\n";
const BUNDLE_LIST_HEADS_USAGE: &str = "usage: git bundle list-heads <file> [<refname>...]\n";
const BUNDLE_UNBUNDLE_USAGE: &str =
    "usage: git bundle unbundle [--progress] <file> [<refname>...]\n";

fn print_bundle_usage() {
    eprint!("{BUNDLE_CREATE_USAGE}");
    eprint!("{BUNDLE_VERIFY_USAGE}");
    eprint!("{BUNDLE_LIST_HEADS_USAGE}");
    eprint!("{BUNDLE_UNBUNDLE_USAGE}");
}

fn bundle_usage_error(usage: &str) -> Result<()> {
    eprintln!("fatal: need a <file> argument");
    eprint!("{usage}");
    Err(GitError::Exit(129))
}
fn cmd_bundle_create(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut progress = false;
    let mut version = None;
    let mut path = None::<String>;
    let mut rev_args = Vec::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if path.is_some() {
            rev_args.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--progress" | "--all-progress" | "--all-progress-implied" | "--no-quiet" => {
                progress = true
            }
            "--no-progress" => progress = false,
            "--version" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("bundle create --version requires a value".into())
                })?;
                version = Some(parse_bundle_version(value)?);
            }
            value if value.starts_with("--version=") => {
                version = Some(parse_bundle_version(&value["--version=".len()..])?);
            }
            value if value.starts_with('-') && value != "-" => {
                return Err(GitError::Command(format!(
                    "unsupported bundle create option {value}"
                )));
            }
            value => path = Some(value.to_string()),
        }
    }
    let Some(path) = path else {
        return bundle_usage_error(BUNDLE_CREATE_USAGE);
    };
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let options = parse_bundle_revision_args(&rev_args)?;
    let selection = bundle_create_selection(
        &git_dir,
        format,
        &db,
        cli_session.replace_objects(),
        &options,
    )?;
    if selection.references.is_empty() {
        eprintln!("fatal: Refusing to create empty bundle.");
        return Err(GitError::Exit(128));
    }
    let Some(pack) =
        build_reachable_pack(&db, format, selection.starts, &selection.excluded_objects)?
    else {
        eprintln!("fatal: Refusing to create empty bundle.");
        return Err(GitError::Exit(128));
    };
    let version = version.unwrap_or(
        if format == ObjectFormat::Sha1 && options.filter.is_none() {
            2
        } else {
            3
        },
    );
    if !(2..=3).contains(&version) {
        return Err(GitError::InvalidFormat(format!(
            "unsupported bundle version {version}"
        )));
    }
    if version == 2 && (format != ObjectFormat::Sha1 || options.filter.is_some()) {
        return Err(GitError::InvalidFormat(format!(
            "cannot write bundle version {version} with algorithm {}",
            format.name()
        )));
    }
    let mut capabilities = Vec::new();
    if version == 3 {
        capabilities.push(BundleCapability {
            key: "object-format".into(),
            value: Some(format.name().as_bytes().to_vec()),
        });
        if let Some(filter) = options.filter {
            capabilities.push(BundleCapability {
                key: "filter".into(),
                value: Some(filter.into_bytes()),
            });
        }
    }
    let bundle = Bundle {
        version,
        format,
        capabilities,
        prerequisites: selection.prerequisites,
        references: selection.references,
        pack: pack.pack,
    };
    let bytes = bundle.write()?;
    if path == "-" {
        io::stdout().write_all(&bytes)?;
    } else {
        fs::write(path, bytes)?;
    }
    if progress && !quiet {
        let count = bundle.references.len();
        eprintln!("Writing objects: 100% ({count}/{count}), done.");
    }
    Ok(())
}

fn cmd_bundle_verify(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut quiet = false;
    let mut path = None;
    for arg in args {
        match arg.as_str() {
            "-q" | "--quiet" if path.is_none() => quiet = true,
            _ if path.is_none() => path = Some(arg),
            _ => {
                return Err(GitError::Command(
                    "bundle verify requires [-q|--quiet] <file>".into(),
                ));
            }
        }
    }
    let Some(path) = path else {
        return bundle_usage_error(BUNDLE_VERIFY_USAGE);
    };
    let git_dir = match cli_session.git_dir() {
        Ok(git_dir) => git_dir,
        Err(_) => {
            eprintln!("error: need a repository to verify a bundle");
            return Err(GitError::Exit(1));
        }
    };
    let format = repository_object_format(&git_dir)?;
    let bundle = Bundle::parse(&read_bundle_path(path)?, format)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    verify_bundle_prerequisites_for_cli(&bundle, &db)?;
    if !quiet {
        print_bundle_verify_details(&bundle)?;
    }
    eprintln!("{} is okay", if path == "-" { "<stdin>" } else { path });
    Ok(())
}

fn cmd_bundle_list_heads(args: &[String]) -> Result<()> {
    let Some(path) = args.first() else {
        return bundle_usage_error(BUNDLE_LIST_HEADS_USAGE);
    };
    let refs = &args[1..];
    let bundle = Bundle::parse_standalone(&read_bundle_path(path)?)?;
    print_bundle_refs(&bundle.references, refs)
}

fn cmd_bundle_unbundle(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut progress = false;
    let mut path = None;
    let mut refs = Vec::new();
    for arg in args {
        if arg == "--progress" && path.is_none() {
            progress = true;
        } else if path.is_none() {
            path = Some(arg);
        } else {
            refs.push(arg.clone());
        }
    }
    let _ = progress;
    let Some(path) = path else {
        return bundle_usage_error(BUNDLE_UNBUNDLE_USAGE);
    };
    let git_dir = match cli_session.git_dir() {
        Ok(git_dir) => git_dir,
        Err(_) => {
            eprintln!("fatal: Need a repository to unbundle.");
            return Err(GitError::Exit(128));
        }
    };
    let format = repository_object_format(&git_dir)?;
    let bundle = Bundle::parse(&read_bundle_path(path)?, format)?;
    let prerequisite_reader = FileObjectDatabase::from_git_dir(&git_dir, format);
    let database = FileObjectDatabase::from_git_dir(&git_dir, format);
    let result = install_bundle_pack(&bundle, &prerequisite_reader, &database)?;
    print_bundle_refs(&result.references, &refs)
}

fn print_bundle_refs(refs: &[BundleReference], filters: &[String]) -> Result<()> {
    for reference in refs {
        if filters.is_empty() || filters.iter().any(|filter| filter == &reference.name) {
            println!("{} {}", reference.oid, reference.name);
        }
    }
    Ok(())
}

fn print_bundle_verify_details(bundle: &Bundle) -> Result<()> {
    match bundle.references.len() {
        1 => println!("The bundle contains this ref:"),
        count => println!("The bundle contains these {count} refs:"),
    }
    print_bundle_refs(&bundle.references, &[])?;
    match bundle.prerequisites.len() {
        0 => println!("The bundle records a complete history."),
        1 => {
            println!("The bundle requires this ref:");
            print_bundle_prerequisites(bundle)?;
        }
        count => {
            println!("The bundle requires these {count} refs:");
            print_bundle_prerequisites(bundle)?;
        }
    }
    println!(
        "The bundle uses this hash algorithm: {}",
        bundle.format.name()
    );
    if let Some(filter) = bundle_filter_capability(bundle)? {
        println!("The bundle uses this filter: {filter}");
    }
    Ok(())
}

fn verify_bundle_prerequisites_for_cli(bundle: &Bundle, db: &FileObjectDatabase) -> Result<()> {
    let mut missing = Vec::new();
    for prerequisite in &bundle.prerequisites {
        match db.read_object(&prerequisite.oid) {
            Ok(object) => {
                let actual = object.object_id(bundle.format)?;
                if actual != prerequisite.oid {
                    return Err(GitError::InvalidObject(format!(
                        "bundle prerequisite {} hashes to {actual}",
                        prerequisite.oid
                    )));
                }
            }
            Err(GitError::NotFound(_)) => missing.push(prerequisite),
            Err(err) => return Err(err),
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    eprintln!("error: Repository lacks these prerequisite commits:");
    for prerequisite in missing {
        eprintln!("error: {} ", prerequisite.oid);
    }
    Err(GitError::Exit(1))
}

fn print_bundle_prerequisites(bundle: &Bundle) -> Result<()> {
    for prerequisite in &bundle.prerequisites {
        println!("{} ", prerequisite.oid);
    }
    Ok(())
}

fn read_bundle_path(path: &str) -> Result<Vec<u8>> {
    if path == "-" {
        let mut bytes = Vec::new();
        io::stdin().read_to_end(&mut bytes)?;
        Ok(bytes)
    } else {
        Ok(fs::read(path)?)
    }
}

fn parse_bundle_version(value: &str) -> Result<u8> {
    value
        .parse::<u8>()
        .map_err(|_| GitError::Command(format!("invalid bundle version {value}")))
}

fn bundle_filter_capability(bundle: &Bundle) -> Result<Option<String>> {
    for capability in &bundle.capabilities {
        if capability.key == "filter" {
            let Some(value) = &capability.value else {
                return Ok(Some(String::new()));
            };
            let text = std::str::from_utf8(value)
                .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
            return Ok(Some(text.to_string()));
        }
    }
    Ok(None)
}

#[derive(Default)]
struct BundleRevisionOptions {
    all: bool,
    ignore_missing: bool,
    max_count: Option<usize>,
    since: Option<i64>,
    filter: Option<String>,
    specs: Vec<String>,
}

fn parse_bundle_revision_args(args: &[String]) -> Result<BundleRevisionOptions> {
    let mut options = BundleRevisionOptions::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--all" => options.all = true,
            "--objects" => {}
            "--stdin" => {
                let mut input = String::new();
                io::stdin().read_to_string(&mut input)?;
                options.specs.extend(
                    input
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(str::to_string),
                );
            }
            "--ignore-missing" => options.ignore_missing = true,
            "--max-count" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--max-count requires a value".into()))?;
                options.max_count = Some(parse_bundle_usize("--max-count", value)?);
            }
            "--since" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--since requires a value".into()))?;
                options.since = parse_bundle_since(value);
            }
            value if value.starts_with("--max-count=") => {
                options.max_count = Some(parse_bundle_usize(
                    "--max-count",
                    &value["--max-count=".len()..],
                )?);
            }
            value if value.starts_with("--since=") => {
                options.since = parse_bundle_since(&value["--since=".len()..]);
            }
            value if value.starts_with("--filter=") => {
                options.filter = Some(value["--filter=".len()..].to_string());
            }
            value => options.specs.push(value.to_string()),
        }
    }
    if !options.all && options.specs.is_empty() {
        return Err(GitError::Unsupported(
            "bundle create currently supports --all or explicit <rev> [^<rev>...]".into(),
        ));
    }
    Ok(options)
}

fn parse_bundle_usize(option: &str, value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("{option} expects a numerical value")))
}

fn parse_bundle_since(value: &str) -> Option<i64> {
    crate::commands::approxidate::parse_commit_date(value).map(|(timestamp, _)| timestamp)
}

fn bundle_all_references(
    git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
) -> Result<Vec<BundleReference>> {
    let store = FileRefStore::new(git_dir, format);
    let mut references = Vec::new();
    for reference in store.list_refs()? {
        if let RefTarget::Direct(oid) = reference.target {
            references.push(BundleReference {
                oid,
                name: reference.name,
            });
        }
    }
    if let Ok(oid) = resolve_revision(git_dir, format, "HEAD", replace_objects) {
        references.push(BundleReference {
            oid,
            name: "HEAD".into(),
        });
    }
    Ok(references)
}

struct BundleCreateSelection {
    references: Vec<BundleReference>,
    prerequisites: Vec<BundlePrerequisite>,
    starts: Vec<ObjectId>,
    excluded_objects: HashSet<ObjectId>,
}

#[derive(Clone)]
struct BundleSpec {
    oid: ObjectId,
    name: String,
    include_ref: bool,
}

fn bundle_create_selection(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    replace_objects: bool,
    options: &BundleRevisionOptions,
) -> Result<BundleCreateSelection> {
    let mut includes = if options.all {
        bundle_all_references(git_dir, format, replace_objects)?
            .into_iter()
            .map(|reference| BundleSpec {
                oid: reference.oid,
                name: reference.name,
                include_ref: true,
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut excludes = Vec::new();
    for spec in &options.specs {
        add_bundle_revision_spec(
            git_dir,
            format,
            db,
            replace_objects,
            spec,
            options.ignore_missing,
            &mut includes,
            &mut excludes,
        )?;
    }

    let user_excluded_objects = collect_reachable_object_ids(db, format, excludes.iter().copied())?;
    let mut references = filter_bundle_references(
        git_dir,
        format,
        db,
        includes,
        options,
        &user_excluded_objects,
    )?;
    dedupe_bundle_references(&mut references);
    let starts = references
        .iter()
        .map(|reference| reference.oid)
        .collect::<Vec<_>>();
    excludes.extend(bundle_limit_excludes(db, format, &starts, options)?);
    let excluded_objects = collect_reachable_object_ids(db, format, excludes.iter().copied())?;
    let mut prerequisites = bundle_boundary_prerequisites(db, format, &starts, &excluded_objects)?;
    order_bundle_prerequisites(db, format, &mut prerequisites, &excludes);
    Ok(BundleCreateSelection {
        references,
        prerequisites,
        starts,
        excluded_objects,
    })
}

fn add_bundle_revision_spec(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    replace_objects: bool,
    spec: &str,
    ignore_missing: bool,
    includes: &mut Vec<BundleSpec>,
    excludes: &mut Vec<ObjectId>,
) -> Result<()> {
    if let Some(excluded) = spec.strip_prefix('^') {
        if excluded.is_empty() {
            return Err(GitError::Command(
                "bundle create excludes require a revision".into(),
            ));
        }
        match resolve_revision(git_dir, format, excluded, replace_objects) {
            Ok(oid) => excludes.push(oid),
            Err(err) if ignore_missing => {
                let _ = err;
            }
            Err(err) => return Err(err),
        }
        return Ok(());
    }
    if let Some(base) = spec.strip_suffix("^!") {
        let oid = resolve_revision(git_dir, format, base, replace_objects)?;
        includes.push(BundleSpec {
            oid,
            name: bundle_display_ref(git_dir, format, base, oid)?,
            include_ref: true,
        });
        if let Ok(object) = db.read_object(&oid)
            && object.object_type == ObjectType::Commit
        {
            for parent in Commit::parse_ref(format, &object.body)?.parents {
                excludes.push(parent);
            }
        }
        return Ok(());
    }
    if let Some((left, right)) = spec.split_once("..")
        && !left.contains("..")
        && !right.contains("..")
    {
        let left = if left.is_empty() { "HEAD" } else { left };
        let right = if right.is_empty() { "HEAD" } else { right };
        excludes.push(resolve_revision(git_dir, format, left, replace_objects)?);
        let oid = resolve_revision(git_dir, format, right, replace_objects)?;
        includes.push(BundleSpec {
            oid,
            name: bundle_display_ref(git_dir, format, right, oid)?,
            include_ref: true,
        });
        return Ok(());
    }
    let oid = match resolve_revision(git_dir, format, spec, replace_objects) {
        Ok(oid) => oid,
        Err(err) if ignore_missing => {
            let _ = err;
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    includes.push(BundleSpec {
        oid,
        name: bundle_display_ref(git_dir, format, spec, oid)?,
        include_ref: true,
    });
    Ok(())
}

fn filter_bundle_references(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    includes: Vec<BundleSpec>,
    options: &BundleRevisionOptions,
    excluded_objects: &HashSet<ObjectId>,
) -> Result<Vec<BundleReference>> {
    let mut refs = includes;
    refs.retain(|reference| {
        if !excluded_objects.contains(&reference.oid) {
            return true;
        }
        db.read_object(&reference.oid)
            .is_ok_and(|object| object.object_type == ObjectType::Tag)
    });
    if let Some(since) = options.since {
        refs.retain(|reference| {
            bundle_object_timestamp(db, format, &reference.oid)
                .is_none_or(|timestamp| timestamp > since)
        });
    }
    if let Some(max_count) = options.max_count {
        let mut commit_refs = refs
            .iter()
            .enumerate()
            .filter_map(|(idx, reference)| {
                let object = db.read_object(&reference.oid).ok()?;
                if object.object_type == ObjectType::Commit {
                    Some((
                        idx,
                        bundle_object_timestamp(db, format, &reference.oid).unwrap_or(0),
                    ))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        commit_refs.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        let keep = commit_refs
            .into_iter()
            .take(max_count)
            .map(|(idx, _)| idx)
            .collect::<HashSet<_>>();
        refs = refs
            .into_iter()
            .enumerate()
            .filter_map(|(idx, reference)| {
                let object = db.read_object(&reference.oid).ok()?;
                if object.object_type == ObjectType::Commit && !keep.contains(&idx) {
                    return None;
                }
                Some(reference)
            })
            .collect();
    }
    refs.into_iter()
        .filter(|reference| reference.include_ref)
        .map(|reference| {
            Ok(BundleReference {
                oid: reference.oid,
                name: if reference.name == "HEAD" {
                    "HEAD".into()
                } else {
                    bundle_display_ref(git_dir, format, &reference.name, reference.oid)?
                },
            })
        })
        .collect()
}

fn dedupe_bundle_references(references: &mut Vec<BundleReference>) {
    let mut seen = HashSet::new();
    references.retain(|reference| seen.insert(reference.name.clone()));
}

fn bundle_display_ref(
    git_dir: &Path,
    format: ObjectFormat,
    spec: &str,
    oid: ObjectId,
) -> Result<String> {
    if spec == "HEAD" {
        return Ok("HEAD".into());
    }
    let store = FileRefStore::new(git_dir, format);
    let refs = store.list_refs()?;
    if spec.starts_with("refs/")
        && refs
            .iter()
            .any(|reference| reference.name == spec && reference.target == RefTarget::Direct(oid))
    {
        return Ok(spec.to_string());
    }
    let branch = format!("refs/heads/{spec}");
    if refs
        .iter()
        .any(|reference| reference.name == branch && reference.target == RefTarget::Direct(oid))
    {
        return Ok(branch);
    }
    let tag = format!("refs/tags/{spec}");
    if refs
        .iter()
        .any(|reference| reference.name == tag && reference.target == RefTarget::Direct(oid))
    {
        return Ok(tag);
    }
    if let Some(reference) = refs
        .iter()
        .find(|reference| reference.target == RefTarget::Direct(oid))
    {
        return Ok(reference.name.clone());
    }
    Ok(spec.to_string())
}

fn bundle_boundary_prerequisites(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: &[ObjectId],
    excluded_objects: &HashSet<ObjectId>,
) -> Result<Vec<BundlePrerequisite>> {
    let mut prerequisites = Vec::new();
    let mut prerequisite_seen = HashSet::new();
    let mut seen = HashSet::new();
    let mut pending = VecDeque::from(starts.to_vec());
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        if excluded_objects.contains(&oid) {
            if bundle_is_commit(db, format, &oid)? && prerequisite_seen.insert(oid) {
                prerequisites.push(BundlePrerequisite {
                    oid,
                    comment: Vec::new(),
                });
            }
            continue;
        }
        let object = db.read_object(&oid)?;
        match object.object_type {
            ObjectType::Commit => {
                for parent in Commit::parse_ref(format, &object.body)?.parents {
                    if excluded_objects.contains(&parent) {
                        if prerequisite_seen.insert(parent) {
                            prerequisites.push(BundlePrerequisite {
                                oid: parent,
                                comment: Vec::new(),
                            });
                        }
                    } else {
                        pending.push_back(parent);
                    }
                }
            }
            ObjectType::Tag => {
                let tag = Tag::parse_ref(format, &object.body)?;
                if !excluded_objects.contains(&tag.object) {
                    pending.push_back(tag.object);
                }
            }
            _ => {}
        }
    }
    Ok(prerequisites)
}

fn bundle_limit_excludes(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: &[ObjectId],
    options: &BundleRevisionOptions,
) -> Result<Vec<ObjectId>> {
    let mut excludes = Vec::new();
    let mut seen = HashSet::new();
    if options.max_count.is_some() {
        for oid in starts {
            let object = db.read_object(oid)?;
            if object.object_type != ObjectType::Commit {
                continue;
            }
            for parent in Commit::parse_ref(format, &object.body)?.parents {
                if seen.insert(parent) {
                    excludes.push(parent);
                }
            }
        }
    }
    if let Some(since) = options.since {
        let mut pending = VecDeque::from(starts.to_vec());
        while let Some(oid) = pending.pop_front() {
            let object = db.read_object(&oid)?;
            match object.object_type {
                ObjectType::Commit => {
                    if bundle_object_timestamp(db, format, &oid).is_some_and(|time| time <= since) {
                        if seen.insert(oid) {
                            excludes.push(oid);
                        }
                        continue;
                    }
                    for parent in Commit::parse_ref(format, &object.body)?.parents {
                        pending.push_back(parent);
                    }
                }
                ObjectType::Tag => {}
                _ => {}
            }
        }
    }
    Ok(excludes)
}

fn order_bundle_prerequisites(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    prerequisites: &mut [BundlePrerequisite],
    exclude_tips: &[ObjectId],
) {
    let exact_rank = exclude_tips
        .iter()
        .enumerate()
        .map(|(idx, oid)| (*oid, idx))
        .collect::<HashMap<_, _>>();
    let has_exact = prerequisites
        .iter()
        .any(|prerequisite| exact_rank.contains_key(&prerequisite.oid));
    prerequisites.sort_by(|left, right| {
        match (
            exact_rank.get(&left.oid).copied(),
            exact_rank.get(&right.oid).copied(),
        ) {
            (Some(left_rank), Some(right_rank)) => left_rank.cmp(&right_rank),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => {
                let left_time = bundle_object_timestamp(db, format, &left.oid).unwrap_or(0);
                let right_time = bundle_object_timestamp(db, format, &right.oid).unwrap_or(0);
                if has_exact {
                    left_time.cmp(&right_time)
                } else {
                    right_time.cmp(&left_time)
                }
            }
        }
    });
}

fn bundle_is_commit(db: &FileObjectDatabase, format: ObjectFormat, oid: &ObjectId) -> Result<bool> {
    let object = db.read_object(oid)?;
    Ok(object.object_type == ObjectType::Commit && Commit::parse_ref(format, &object.body).is_ok())
}

fn bundle_object_timestamp(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Option<i64> {
    let object = db.read_object(oid).ok()?;
    match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse_ref(format, &object.body).ok()?;
            bundle_identity_timestamp(commit.committer)
        }
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body).ok()?;
            tag.tagger.and_then(bundle_identity_timestamp)
        }
        _ => None,
    }
}

fn bundle_identity_timestamp(identity: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(identity).ok()?;
    let (before_tz, _) = text.rsplit_once(' ')?;
    let (_, timestamp) = before_tz.rsplit_once(' ')?;
    timestamp.parse::<i64>().ok()
}
