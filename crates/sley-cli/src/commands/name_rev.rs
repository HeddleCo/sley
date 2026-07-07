//! `git name-rev` — find symbolic names for given revs.
//!
//! Names each requested commit by the closest ref that reaches it, expressed in
//! `git rev-parse` syntax (`tag~3`, `branch~2^2`, `tags/v1^0`, ...). The naming
//! algorithm mirrors upstream `builtin/name-rev.c`: every eligible ref seeds a
//! first-parent-preferring walk, each commit keeps the "best" name under a
//! tag-then-distance-then-date ordering, and the `--refs`/`--exclude`/`--tags`
//! filters plus the `--all`/`--annotate-stdin`/positional input modes match the
//! real command's output and exit codes.
#![allow(clippy::expect_used)]

use sley::plumbing::{sley_rev};
// Glob the crate root for shared plumbing; see commands::stash for rationale.
use crate::commands::cli_options::opt_bool;
use crate::*;
use sley_options::{parse_options, OptionName, OptionSpec, ParsedValue};

/// `MERGE_TRAVERSAL_WEIGHT` from upstream: crossing into a non-first parent is
/// treated as a very long hop so first-parent ancestry is strongly preferred.
const MERGE_TRAVERSAL_WEIGHT: i64 = 65535;

/// Default abbreviation length used by `--always` (upstream `DEFAULT_ABBREV`).
const DEFAULT_ABBREV: usize = 7;

/// A ref selected as a starting point ("tip") for naming.
struct Tip {
    oid: ObjectId,
    /// Display name for the ref after prefix shortening (e.g. `tags/v1`, `main`).
    refname: String,
    /// The commit this tip resolves to (after peeling tag objects), if any.
    commit: Option<ObjectId>,
    /// Tag date for annotated tags, else the commit date; `i64::MAX` when unknown.
    taggerdate: i64,
    /// Whether the ref lives under `refs/tags/`.
    from_tag: bool,
    /// Whether the ref pointed at a tag object that had to be dereferenced.
    deref: bool,
}

/// Commit headers used by `name-rev`, cached per command invocation.
#[derive(Clone)]
struct CommitMetadata {
    parents: Vec<ObjectId>,
    committerdate: i64,
}

#[derive(Default)]
struct CommitMetadataCache {
    commits: HashMap<ObjectId, CommitMetadata>,
}

impl CommitMetadataCache {
    fn get_cached(&self, oid: &ObjectId) -> Option<&CommitMetadata> {
        self.commits.get(oid)
    }

    fn get_or_read(
        &mut self,
        db: &FileObjectDatabase,
        format: ObjectFormat,
        oid: &ObjectId,
    ) -> Result<Option<&CommitMetadata>> {
        if !self.commits.contains_key(oid) {
            let object = db.read_object(oid)?;
            if object.object_type != ObjectType::Commit {
                return Ok(None);
            }
            let commit = Commit::parse(format, &object.body)?;
            self.commits.insert(
                *oid,
                CommitMetadata {
                    parents: commit.parents,
                    committerdate: committer_timestamp(&commit.committer).unwrap_or(i64::MAX),
                },
            );
        }
        Ok(self.commits.get(oid))
    }

    fn get_or_parse_commit(
        &mut self,
        format: ObjectFormat,
        oid: &ObjectId,
        body: &[u8],
    ) -> Result<&CommitMetadata> {
        if !self.commits.contains_key(oid) {
            let commit = Commit::parse(format, body)?;
            self.commits.insert(
                *oid,
                CommitMetadata {
                    parents: commit.parents,
                    committerdate: committer_timestamp(&commit.committer).unwrap_or(i64::MAX),
                },
            );
        }
        Ok(self
            .commits
            .get(oid)
            .expect("commit metadata was inserted or already cached"))
    }
}

/// The best name discovered for a commit during the walk.
#[derive(Clone)]
struct RevName {
    tip_name: String,
    generation: i64,
    distance: i64,
    taggerdate: i64,
    from_tag: bool,
}

/// Parsed command-line options for `name-rev`.
struct NameRevOptions {
    name_only: bool,
    tags_only: bool,
    ref_filters: Vec<String>,
    exclude_filters: Vec<String>,
    all: bool,
    annotate_stdin: bool,
    stdin_deprecated: bool,
    allow_undefined: bool,
    always: bool,
    peel_tag: bool,
    revs: Vec<String>,
}

const NAME_REV_USAGE_LINES: &[&str] = &["git name-rev [--tags] [--refs=<pattern>] [options] <commit>..."];

fn name_rev_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(
            None,
            Some("name-only"),
            sley_options::OptFlags::NONE,
            "print name only",
        ),
        opt_bool(
            None,
            Some("tags"),
            sley_options::OptFlags::NONE,
            "only use tags",
        ),
        opt_bool(None, Some("all"), sley_options::OptFlags::NONE, "list all commits"),
        opt_bool(
            None,
            Some("annotate-stdin"),
            sley_options::OptFlags::NONE,
            "annotate stdin",
        ),
        opt_bool(
            None,
            Some("stdin"),
            sley_options::OptFlags::NONEG,
            "deprecated alias for --annotate-stdin",
        ),
        opt_bool(
            None,
            Some("undefined"),
            sley_options::OptFlags::NONE,
            "allow undefined names",
        ),
        opt_bool(
            None,
            Some("always"),
            sley_options::OptFlags::NONE,
            "abbreviate if no name found",
        ),
        opt_bool(
            None,
            Some("peel-tag"),
            sley_options::OptFlags::NONE,
            "peel tags",
        ),
        sley_options::OptionSpec {
            short: None,
            long: Some("refs"),
            value: sley_options::OptValue::Str("pattern"),
            flags: sley_options::OptFlags::NONE,
            help: "only use refs matching pattern",
        },
        sley_options::OptionSpec {
            short: None,
            long: Some("exclude"),
            value: sley_options::OptValue::Str("pattern"),
            flags: sley_options::OptFlags::NONE,
            help: "exclude refs matching pattern",
        },
    ];
    SPECS
}

pub(crate) fn cmd_name_rev(args: &[String]) -> Result<()> {
    let options = setup_name_rev_options(args)?;

    // Upstream rejects mixing an explicit list with the whole-graph modes.
    if (options.all || options.annotate_stdin) && !options.revs.is_empty() {
        eprintln!("error: Specify either a list, or --all, not both!");
        print_name_rev_usage();
        return Err(GitError::Exit(129));
    }

    if options.stdin_deprecated {
        eprintln!(
            "warning: --stdin is deprecated. Please use --annotate-stdin instead, which is functionally equivalent."
        );
        eprintln!("This option will be removed in a future release.");
    }

    let repo = RepositoryContext::discover_current()?;
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();

    let mut commit_cache = CommitMetadataCache::default();
    let tips = collect_tips(git_dir, format, db, &options, &mut commit_cache)?;
    let mut rev_names: HashMap<ObjectId, RevName> = HashMap::new();
    name_all_tips(db, format, &tips, &mut rev_names, &mut commit_cache)?;

    if options.all {
        return emit_all(db, &rev_names, &options);
    }
    if options.annotate_stdin {
        return annotate_stdin(db, format, &tips, &rev_names, &options);
    }
    emit_positional(&repo, &tips, &rev_names, &options)
}

fn setup_name_rev_options(args: &[String]) -> Result<NameRevOptions> {
    if args
        .iter()
        .any(|arg| arg == "-h" || arg == "--help")
    {
        print_name_rev_help();
        return Err(GitError::Exit(129));
    }
    let parsed = match parse_options(args, name_rev_option_specs(), NAME_REV_USAGE_LINES) {
        Ok(parsed) => parsed,
        Err(error) => {
            // git's parse-options prints the usage only for unknown
            // option/switch errors; value errors ("requires a value", "takes no
            // value") emit the error line alone. Match that split.
            let mut print_usage = false;
            if let Some(message) = error.message() {
                if message.starts_with("unknown option `") {
                    let option = message
                        .strip_prefix("unknown option `")
                        .and_then(|rest| rest.strip_suffix('\''))
                        .unwrap_or(message);
                    eprintln!("error: unknown option `{option}'");
                    print_usage = true;
                } else if message.starts_with("unknown switch `") {
                    let option = message
                        .strip_prefix("unknown switch `")
                        .and_then(|rest| rest.strip_suffix('\''))
                        .unwrap_or(message);
                    eprintln!("error: unknown switch `{option}'");
                    print_usage = true;
                } else {
                    eprintln!("error: {message}");
                }
            }
            if print_usage {
                print_name_rev_usage();
            }
            return Err(GitError::Exit(129));
        }
    };
    let mut ref_filters = Vec::new();
    let mut exclude_filters = Vec::new();
    let mut stdin_deprecated = false;
    for option in &parsed.options {
        match option.long {
            Some("refs") => match option.name {
                OptionName::NegatedLong(_) => ref_filters.clear(),
                _ => {
                    if let ParsedValue::Str(value) = option.value {
                        ref_filters.push(value.to_string());
                    }
                }
            },
            Some("exclude") => match option.name {
                OptionName::NegatedLong(_) => exclude_filters.clear(),
                _ => {
                    if let ParsedValue::Str(value) = option.value {
                        exclude_filters.push(value.to_string());
                    }
                }
            },
            Some("stdin") => stdin_deprecated = true,
            _ => {}
        }
    }
    let annotate_stdin = parsed.last_bool("annotate-stdin", false) || stdin_deprecated;
    Ok(NameRevOptions {
        name_only: parsed.last_bool("name-only", false),
        tags_only: parsed.last_bool("tags", false),
        ref_filters,
        exclude_filters,
        all: parsed.last_bool("all", false),
        annotate_stdin,
        stdin_deprecated,
        allow_undefined: parsed.last_bool("undefined", true),
        always: parsed.last_bool("always", false),
        peel_tag: parsed.last_bool("peel-tag", false),
        revs: parsed
            .positionals
            .iter()
            .map(|rev| (*rev).to_string())
            .collect(),
    })
}

/// Build the table of tips from the ref store, honoring `--tags`/`--refs`/`--exclude`.
fn collect_tips(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: &NameRevOptions,
    commit_cache: &mut CommitMetadataCache,
) -> Result<Vec<Tip>> {
    let store = FileRefStore::new(git_dir, format);
    let mut refs = store.list_refs()?;
    // `list_refs` already yields refs sorted by name; keep that order so ties in
    // the tag/age sort below resolve alphabetically, matching how upstream's
    // `for_each_ref` feeds the (otherwise unstable) tip sort.
    refs.sort_by(|left, right| left.name.cmp(&right.name));

    let mut tips = Vec::new();
    for reference in refs {
        let RefTarget::Direct(oid) = reference.target else {
            // Symbolic refs (e.g. a packed HEAD) are not naming tips.
            continue;
        };
        let name = &reference.name;
        if options.tags_only && !name.starts_with("refs/tags/") {
            continue;
        }
        if options
            .exclude_filters
            .iter()
            .any(|pattern| subpath_matches(name, pattern).is_some())
        {
            continue;
        }
        let mut can_abbreviate_output = options.tags_only && options.name_only;
        if !options.ref_filters.is_empty() {
            let mut matched = false;
            for pattern in &options.ref_filters {
                match subpath_matches(name, pattern) {
                    None => {}
                    Some(0) => matched = true,
                    Some(_) => {
                        matched = true;
                        can_abbreviate_output = true;
                    }
                }
            }
            if !matched {
                continue;
            }
        }

        // Peel tag objects through to the underlying commit, recording the tag
        // date for the age-based preference between competing tags.
        let mut current = oid;
        let mut deref = false;
        let mut taggerdate = i64::MAX;
        let mut commit = None;
        loop {
            if let Some(metadata) = commit_cache.get_cached(&current) {
                if taggerdate == i64::MAX {
                    taggerdate = metadata.committerdate;
                }
                commit = Some(current);
                break;
            }
            let object = db.read_object(&current)?;
            match object.object_type {
                ObjectType::Commit => {
                    let metadata =
                        commit_cache.get_or_parse_commit(format, &current, &object.body)?;
                    if taggerdate == i64::MAX {
                        taggerdate = metadata.committerdate;
                    }
                    commit = Some(current);
                    break;
                }
                ObjectType::Tag => {
                    let tag = Tag::parse(format, &object.body)?;
                    if let Some(tagger) = &tag.tagger
                        && let Some(date) = committer_timestamp(tagger)
                    {
                        taggerdate = date;
                    }
                    deref = true;
                    current = tag.object.clone();
                }
                _ => break,
            }
        }

        let from_tag = name.starts_with("refs/tags/");
        let display = if can_abbreviate_output {
            shorten_unambiguous_ref(name)
        } else if let Some(rest) = name.strip_prefix("refs/heads/") {
            rest.to_string()
        } else if let Some(rest) = name.strip_prefix("refs/") {
            rest.to_string()
        } else {
            name.clone()
        };

        tips.push(Tip {
            oid,
            refname: display,
            commit,
            taggerdate,
            from_tag,
            deref,
        });
    }
    Ok(tips)
}

/// Seed a naming walk from every tip in upstream's `cmp_by_tag_and_age` order.
fn name_all_tips(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tips: &[Tip],
    rev_names: &mut HashMap<ObjectId, RevName>,
    commit_cache: &mut CommitMetadataCache,
) -> Result<()> {
    let mut order: Vec<usize> = (0..tips.len()).collect();
    // Stable sort over the alphabetically-ordered tips: tags first, then older
    // dates first; equal keys keep the alphabetical input order.
    order.sort_by(|&left, &right| {
        let a = &tips[left];
        let b = &tips[right];
        b.from_tag
            .cmp(&a.from_tag)
            .then_with(|| a.taggerdate.cmp(&b.taggerdate))
    });
    for index in order {
        let tip = &tips[index];
        let Some(commit) = &tip.commit else {
            continue;
        };
        name_rev(db, format, commit, tip, rev_names, commit_cache)?;
    }
    Ok(())
}

/// Walk first-parent-first from a tip, recording the best name for each commit.
fn name_rev(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    start: &ObjectId,
    tip: &Tip,
    rev_names: &mut HashMap<ObjectId, RevName>,
    commit_cache: &mut CommitMetadataCache,
) -> Result<()> {
    let tip_name = if tip.deref {
        format!("{}^0", tip.refname)
    } else {
        tip.refname.clone()
    };
    if !create_or_update_name(rev_names, start, tip.taggerdate, 0, 0, tip.from_tag) {
        return Ok(());
    }
    if let Some(name) = rev_names.get_mut(start) {
        name.tip_name = tip_name;
    }

    let mut stack = vec![start.clone()];
    while let Some(oid) = stack.pop() {
        let Some(current) = rev_names.get(&oid).cloned() else {
            continue;
        };
        let Some(commit) = commit_cache.get_or_read(db, format, &oid)? else {
            continue;
        };
        // Push parents so the first parent is processed before the others, just
        // like upstream's two-stack arrangement.
        let mut to_queue = Vec::new();
        for (index, parent) in commit.parents.iter().enumerate() {
            let parent_number = index + 1;
            let (generation, distance) = if parent_number > 1 {
                (0, current.distance + MERGE_TRAVERSAL_WEIGHT)
            } else {
                (current.generation + 1, current.distance + 1)
            };
            if create_or_update_name(
                rev_names,
                parent,
                tip.taggerdate,
                generation,
                distance,
                tip.from_tag,
            ) {
                let parent_tip_name = if parent_number > 1 {
                    get_parent_name(&current, parent_number)
                } else {
                    current.tip_name.clone()
                };
                if let Some(name) = rev_names.get_mut(parent) {
                    name.tip_name = parent_tip_name;
                }
                to_queue.push(parent);
            }
        }
        while let Some(parent) = to_queue.pop() {
            stack.push(*parent);
        }
    }
    Ok(())
}

/// Insert or replace the name for `commit` when the candidate is strictly better.
/// Returns whether the slot was (re)claimed, signalling that the walk should
/// descend through this commit's parents.
fn create_or_update_name(
    rev_names: &mut HashMap<ObjectId, RevName>,
    commit: &ObjectId,
    taggerdate: i64,
    generation: i64,
    distance: i64,
    from_tag: bool,
) -> bool {
    if let Some(existing) = rev_names.get(commit)
        && !is_better_name(existing, taggerdate, generation, distance, from_tag)
    {
        return false;
    }
    rev_names.insert(
        commit.clone(),
        RevName {
            tip_name: String::new(),
            generation,
            distance,
            taggerdate,
            from_tag,
        },
    );
    true
}

/// Upstream `is_better_name`: tags beat non-tags; otherwise prefer the smaller
/// effective distance, then the older date.
fn is_better_name(
    name: &RevName,
    taggerdate: i64,
    generation: i64,
    distance: i64,
    from_tag: bool,
) -> bool {
    let name_distance = effective_distance(name.distance, name.generation);
    let new_distance = effective_distance(distance, generation);
    if from_tag && name.from_tag {
        return name_distance > new_distance;
    }
    if name.from_tag != from_tag {
        return from_tag;
    }
    if name_distance != new_distance {
        return name_distance > new_distance;
    }
    if name.taggerdate != taggerdate {
        return name.taggerdate > taggerdate;
    }
    false
}

fn effective_distance(distance: i64, generation: i64) -> i64 {
    distance
        + if generation > 0 {
            MERGE_TRAVERSAL_WEIGHT
        } else {
            0
        }
}

/// Build a non-first-parent's name: strip a trailing `^0`, fold in the run of
/// first-parent steps as `~<generation>`, then append `^<parent_number>`.
fn get_parent_name(name: &RevName, parent_number: usize) -> String {
    let base = name.tip_name.strip_suffix("^0").unwrap_or(&name.tip_name);
    if name.generation > 0 {
        format!("{base}~{}^{parent_number}", name.generation)
    } else {
        format!("{base}^{parent_number}")
    }
}

/// Render a commit's stored name, collapsing the `^0`/`~<generation>` suffixes
/// exactly as upstream's `get_rev_name`.
fn rev_name_string(name: &RevName) -> String {
    if name.generation == 0 {
        name.tip_name.clone()
    } else {
        let base = name.tip_name.strip_suffix("^0").unwrap_or(&name.tip_name);
        format!("{base}~{}", name.generation)
    }
}

/// `--all`: one line per named commit. Upstream prints `<full-oid> <name>`
/// (or just `<name>` with `--name-only`) in an unspecified hash-map order; we
/// emit a deterministic listing sorted by object id.
fn emit_all(
    db: &FileObjectDatabase,
    rev_names: &HashMap<ObjectId, RevName>,
    options: &NameRevOptions,
) -> Result<()> {
    let mut rows: Vec<(ObjectId, RevName)> = rev_names
        .iter()
        .map(|(oid, name)| (*oid, name.clone()))
        .collect();
    rows.sort_by_key(|left| left.0.to_hex());
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (oid, name) in rows {
        // Every commit in the table is reachable, so it always has a name; the
        // `undefined`/`--always` branches of `show_name` never trigger here.
        show_name(
            &mut out,
            db,
            None,
            &oid,
            Some(rev_name_string(&name)),
            options,
        )?;
    }
    Ok(())
}

/// `--annotate-stdin`/`--stdin`: substitute full-length hex runs with names.
fn annotate_stdin(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tips: &[Tip],
    rev_names: &HashMap<ObjectId, RevName>,
    options: &NameRevOptions,
) -> Result<()> {
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let hex_len = format.hex_len();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let context = NameRevAnnotationContext {
        hex_len,
        db,
        format,
        tips,
        rev_names,
        options,
    };
    // Process line by line so a hash split across the buffer boundary is treated
    // the same way upstream's per-line `name_rev_line` would.
    let mut start = 0;
    while start < input.len() {
        let end = match input[start..].iter().position(|byte| *byte == b'\n') {
            Some(offset) => start + offset + 1,
            None => input.len(),
        };
        annotate_stdin_line(&input[start..end], &context, &mut out)?;
        start = end;
    }
    Ok(())
}

struct NameRevAnnotationContext<'a> {
    hex_len: usize,
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    tips: &'a [Tip],
    rev_names: &'a HashMap<ObjectId, RevName>,
    options: &'a NameRevOptions,
}

fn annotate_stdin_line(
    line: &[u8],
    context: &NameRevAnnotationContext<'_>,
    out: &mut impl Write,
) -> Result<()> {
    let mut segment_start = 0;
    let mut counter = 0usize;
    let mut index = 0;
    while index < line.len() {
        let byte = line[index];
        if !is_lower_hex(byte) {
            counter = 0;
        } else {
            counter += 1;
            let next_is_hex = line.get(index + 1).is_some_and(|next| is_lower_hex(*next));
            if counter == context.hex_len && !next_is_hex {
                let hex_start = index + 1 - context.hex_len;
                let hex = &line[hex_start..=index];
                counter = 0;
                if let Some(name) = resolve_stdin_name(
                    hex,
                    context.db,
                    context.format,
                    context.tips,
                    context.rev_names,
                    context.options,
                )? {
                    if context.options.name_only {
                        out.write_all(&line[segment_start..hex_start])?;
                        out.write_all(name.as_bytes())?;
                    } else {
                        out.write_all(&line[segment_start..=index])?;
                        out.write_all(format!(" ({name})").as_bytes())?;
                    }
                    segment_start = index + 1;
                }
            }
        }
        index += 1;
    }
    out.write_all(&line[segment_start..])?;
    Ok(())
}

/// Look up the name for a hex string found in stdin. The bytes are valid ASCII
/// hex of the right length by construction.
fn resolve_stdin_name(
    hex: &[u8],
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tips: &[Tip],
    rev_names: &HashMap<ObjectId, RevName>,
    options: &NameRevOptions,
) -> Result<Option<String>> {
    let Ok(text) = std::str::from_utf8(hex) else {
        return Ok(None);
    };
    let Ok(oid) = ObjectId::from_hex(format, text) else {
        return Ok(None);
    };
    // Only objects that actually exist are eligible for substitution.
    if db.read_object(&oid).is_err() {
        return Ok(None);
    }
    name_for_object(&oid, db, format, tips, rev_names, options)
}

/// `<commit>...`: resolve each argument and print its name (or `undefined`).
fn emit_positional(
    repo: &RepositoryContext,
    tips: &[Tip],
    rev_names: &HashMap<ObjectId, RevName>,
    options: &NameRevOptions,
) -> Result<()> {
    let format = repo.format();
    let db = repo.objects();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for rev in &options.revs {
        let oid = match repo.resolve_revision(rev) {
            Ok(oid) => oid,
            Err(_) => {
                eprintln!("Could not get sha1 for {rev}. Skipping.");
                continue;
            }
        };
        if db.read_object(&oid).is_err() {
            eprintln!("Could not get object for {rev}. Skipping.");
            continue;
        }
        let object_for_name = if options.peel_tag {
            match sley_rev::peel_to_commit(db, format, &oid) {
                Ok(commit) => commit,
                Err(_) => {
                    eprintln!("Could not get commit for {rev}. Skipping.");
                    continue;
                }
            }
        } else {
            oid
        };
        let name = name_for_object(&object_for_name, db, format, tips, rev_names, options)?;
        show_name(&mut out, db, Some(rev), &oid, name, options)?;
    }
    Ok(())
}

/// Resolve the displayed name for an object: commits use the walk result;
/// non-commits fall back to an exact ref (tip) match by oid.
fn name_for_object(
    oid: &ObjectId,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tips: &[Tip],
    rev_names: &HashMap<ObjectId, RevName>,
    options: &NameRevOptions,
) -> Result<Option<String>> {
    // The `--tags --name-only` `tags/`-prefix omission is already applied when
    // building tip names (via `can_abbreviate_output` in `collect_tips`), so the
    // stored names need no further adjustment here — matching upstream, where
    // `--name-only` only suppresses the leading object id.
    let _ = options;
    let object = db.read_object(oid)?;
    if object.object_type == ObjectType::Commit {
        return Ok(rev_names.get(oid).map(rev_name_string));
    }
    let _ = format;
    Ok(exact_tip_match(oid, tips))
}

/// Print one positional/`--all` style result line, applying `--name-only`,
/// `--always`, and the `--no-undefined` "cannot describe" failure.
fn show_name(
    out: &mut impl Write,
    db: &FileObjectDatabase,
    caller_name: Option<&str>,
    oid: &ObjectId,
    name: Option<String>,
    options: &NameRevOptions,
) -> Result<()> {
    if !options.name_only {
        let label = caller_name
            .map(str::to_string)
            .unwrap_or_else(|| oid.to_hex());
        write!(out, "{label} ")?;
    }
    if let Some(name) = name {
        writeln!(out, "{name}")?;
    } else if options.allow_undefined {
        writeln!(out, "undefined")?;
    } else if options.always {
        writeln!(out, "{}", find_unique_abbrev(db, oid)?)?;
    } else {
        // Match upstream ordering: the partial line above is already flushed, so
        // the fatal goes to stderr and we exit non-zero without a trailing name.
        out.flush()?;
        eprintln!("fatal: cannot describe '{}'", oid.to_hex());
        return Err(GitError::Exit(128));
    }
    Ok(())
}

/// Find the tip whose oid equals `oid` and return its display name.
fn exact_tip_match(oid: &ObjectId, tips: &[Tip]) -> Option<String> {
    tips.iter()
        .find(|tip| &tip.oid == oid)
        .map(|tip| tip.refname.clone())
}

/// `--always` fallback: shortest unique hex prefix, at least `DEFAULT_ABBREV`,
/// grown until it resolves to a single object (matching `find_unique_abbrev`).
fn find_unique_abbrev(db: &FileObjectDatabase, oid: &ObjectId) -> Result<String> {
    let hex = oid.to_hex();
    let mut width = DEFAULT_ABBREV.min(hex.len());
    while width < hex.len() {
        match db.resolve_prefix(&hex[..width])? {
            ObjectPrefixResolution::Unique(_) | ObjectPrefixResolution::Missing => break,
            ObjectPrefixResolution::Ambiguous(_) => width += 1,
        }
    }
    Ok(hex[..width].to_string())
}

/// Match `filter` against `path` and each `/`-delimited suffix, returning the
/// offset (in chars) of the matched suffix, mirroring upstream `subpath_matches`.
fn subpath_matches(path: &str, filter: &str) -> Option<usize> {
    let mut offset = 0;
    loop {
        let subpath = &path[offset..];
        if wildmatch(filter, subpath) {
            return Some(offset);
        }
        match subpath.find('/') {
            Some(slash) => offset += slash + 1,
            None => return None,
        }
    }
}

/// Shorten a fully-qualified ref to its unambiguous short form, matching the
/// common cases of git's `shorten_unambiguous_ref` (the only forms `name-rev`
/// produces): branches, tags, and remote-tracking refs.
fn shorten_unambiguous_ref(refname: &str) -> String {
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(rest) = refname.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    refname.strip_prefix("refs/").unwrap_or(refname).to_string()
}

/// Shell-glob match (`*`, `?`, `[...]`) over the whole string. `*` matches `/`
/// because upstream calls `wildmatch` with no `WM_PATHNAME` flag.
fn wildmatch(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    wildmatch_inner(&pattern, &text)
}

fn wildmatch_inner(pattern: &[char], text: &[char]) -> bool {
    let mut p = 0;
    let mut t = 0;
    // Backtracking position for the most recent `*`.
    let mut star_pattern: Option<usize> = None;
    let mut star_text = 0;
    while t < text.len() {
        if p < pattern.len() {
            match pattern[p] {
                '*' => {
                    star_pattern = Some(p);
                    star_text = t;
                    p += 1;
                    continue;
                }
                '?' => {
                    p += 1;
                    t += 1;
                    continue;
                }
                '[' => {
                    if let Some((matched, next)) = match_bracket(pattern, p, text[t]) {
                        if matched {
                            p = next;
                            t += 1;
                            continue;
                        }
                    } else if pattern[p] == text[t] {
                        // Malformed class: treat `[` literally.
                        p += 1;
                        t += 1;
                        continue;
                    }
                }
                '\\' if p + 1 < pattern.len() && pattern[p + 1] == text[t] => {
                    p += 2;
                    t += 1;
                    continue;
                }
                other if other == text[t] => {
                    p += 1;
                    t += 1;
                    continue;
                }
                _ => {}
            }
        }
        // Mismatch: backtrack to the last `*` if there was one.
        if let Some(star) = star_pattern {
            p = star + 1;
            star_text += 1;
            t = star_text;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Try to match a `[...]` bracket expression at `pattern[start]` against `ch`.
/// Returns `Some((matched, index_after_class))`, or `None` if the class is
/// malformed (no closing `]`).
fn match_bracket(pattern: &[char], start: usize, ch: char) -> Option<(bool, usize)> {
    let mut index = start + 1;
    let mut negate = false;
    if index < pattern.len() && (pattern[index] == '!' || pattern[index] == '^') {
        negate = true;
        index += 1;
    }
    let mut matched = false;
    let mut first = true;
    while index < pattern.len() {
        if pattern[index] == ']' && !first {
            let result = matched ^ negate;
            return Some((result, index + 1));
        }
        first = false;
        // Range like `a-z` (not when `-` is the final char before `]`).
        if index + 2 < pattern.len() && pattern[index + 1] == '-' && pattern[index + 2] != ']' {
            let low = pattern[index];
            let high = pattern[index + 2];
            if low <= ch && ch <= high {
                matched = true;
            }
            index += 3;
        } else {
            if pattern[index] == ch {
                matched = true;
            }
            index += 1;
        }
    }
    None
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

/// Parse the trailing `<unix-seconds> <tz>` of a committer/tagger identity line.
fn committer_timestamp(ident: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(ident).ok()?;
    let close = text.rfind('>')?;
    let rest = text[close + 1..].trim();
    let seconds = rest.split_whitespace().next()?;
    seconds.parse::<i64>().ok()
}

fn print_name_rev_help() {
    print!("{NAME_REV_HELP}");
}

fn print_name_rev_usage() {
    eprint!("{NAME_REV_HELP}");
}

const NAME_REV_HELP: &str = "usage: git name-rev [<options>] <commit>...\n   or: git name-rev [<options>] --all\n   or: git name-rev [<options>] --annotate-stdin\n\n    --[no-]name-only      print only ref-based names (no object names)\n    --[no-]tags           only use tags to name the commits\n    --[no-]refs <pattern> only use refs matching <pattern>\n    --[no-]exclude <pattern>\n                          ignore refs matching <pattern>\n\n    --[no-]all            list all commits reachable from all refs\n    --[no-]annotate-stdin annotate text from stdin\n    --[no-]undefined      allow to print `undefined` names (default)\n    --[no-]always         show abbreviated commit object as fallback\n\n";
