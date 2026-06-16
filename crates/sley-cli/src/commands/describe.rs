//! `git describe`: name a commit relative to the most recent reachable tag.
//!
//! The output for a commit that is not itself tagged is `<tag>-<n>-g<short>`,
//! where `<n>` is the number of commits between the commit and the tag and
//! `<short>` is the abbreviated commit object name. When the commit is exactly
//! a tag, the bare tag name is printed (unless `--long` forces the long form).
//!
//! The selection algorithm mirrors upstream `git describe`: tags are gathered
//! (one best tag per commit), then a commit-date-ordered priority walk from the
//! target registers candidates as it reaches their tagged commits. A per-candidate
//! flag is propagated to ancestors so each candidate's depth equals the count of
//! commits reachable from the target but not from the candidate's tagged commit
//! (`git rev-list <target> ^<tag>`). The winner is the candidate with the
//! smallest depth, ties broken by registration order (which follows commit date).

// Glob the crate root for shared plumbing; see commands::stash for rationale.
use crate::*;

/// The default number of most-recent tags to consider, matching git.
const DEFAULT_CANDIDATES: usize = 10;
/// The smallest abbreviation length git will ever emit for an object name.
const MINIMUM_ABBREV: usize = 4;

/// Compute the `%(describe[:opts])` string for a commit, mirroring git's
/// `format_commit_one`/`describe` integration: failures (no names, no candidate)
/// yield `Ok(None)` so the placeholder expands to an empty string instead of
/// erroring. `tags`/`abbrev`/`matches`/`excludes` come from the `%(describe:...)`
/// option parse.
pub(crate) fn describe_for_format(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    target: &ObjectId,
    use_tags: bool,
    abbrev_opt: Option<usize>,
    matches: &[String],
    excludes: &[String],
) -> Result<Option<String>> {
    let mut options = DescribeOptions {
        tags: use_tags,
        ..DescribeOptions::default()
    };
    options.patterns.extend(matches.iter().cloned());
    options.exclude_patterns.extend(excludes.iter().cloned());
    if let Some(abbrev) = abbrev_opt {
        options.abbrev = Some(abbrev);
    }

    let abbrev = resolve_describe_abbrev(git_dir, format, options.abbrev)?;
    let tags = collect_describe_tags(git_dir, format, db, &options)?;
    if tags.by_commit.is_empty() {
        return Ok(None);
    }

    // git describe peels the requested object to a commit first (e.g. the
    // %(describe) atom on an annotated tag is asked to describe the tag object).
    // The by-commit map and the `-g<oid>` suffix are keyed on the commit, so
    // resolve it once here.
    let object = db.read_object(target)?;
    let target = &match object.object_type {
        ObjectType::Commit => *target,
        ObjectType::Tag => sley_rev::peel_to_commit(db, format, target)?,
        _ => return Ok(None),
    };

    // Exact match.
    if let Some(best) = tags
        .by_commit
        .get(target)
        .filter(|tag| describe_eligible(tag, &options))
    {
        if options.long {
            return Ok(Some(format!(
                "{}-0-g{}",
                best.name,
                describe_abbrev_oid(db, target, abbrev)?
            )));
        }
        return Ok(Some(best.name.clone()));
    }

    let search = describe_search(format, db, &options, &tags.by_commit, target)?;
    let Some((best, _traversed)) = search.found else {
        return Ok(None);
    };
    let short = describe_abbrev_oid(db, target, abbrev)?;
    if options.long || best.depth != 0 {
        if abbrev == 0 {
            Ok(Some(best.tag.name.clone()))
        } else {
            Ok(Some(format!("{}-{}-g{short}", best.tag.name, best.depth)))
        }
    } else {
        Ok(Some(best.tag.name.clone()))
    }
}

pub(crate) fn cmd_describe(args: &[String]) -> Result<()> {
    let mut options = DescribeOptions::default();
    let mut commits: Vec<String> = Vec::new();
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if positional_only {
            commits.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-h" | "--help" => {
                print_describe_usage(&mut io::stdout());
                return Err(GitError::Exit(129));
            }
            "--all" => options.all = true,
            "--no-all" => options.all = false,
            "--tags" => options.tags = true,
            "--no-tags" => options.tags = false,
            "--long" => options.long = true,
            "--no-long" => options.long = false,
            // `--exact-match` is an alias for `--candidates=0`.
            "--exact-match" => options.max_candidates = 0,
            "--no-exact-match" => options.max_candidates = DEFAULT_CANDIDATES,
            "--first-parent" => options.first_parent = true,
            "--no-first-parent" => options.first_parent = false,
            "--always" => options.always = true,
            "--no-always" => options.always = false,
            "--debug" => options.debug = true,
            "--no-debug" => options.debug = false,
            "--contains" => options.contains = true,
            "--no-contains" => options.contains = false,
            "--dirty" => options.dirty = Some("-dirty".to_string()),
            "--no-dirty" => options.dirty = None,
            "--broken" => options.broken = Some("-broken".to_string()),
            "--no-broken" => options.broken = None,
            "--abbrev" => options.abbrev = Some(DEFAULT_ABBREV_SENTINEL),
            "--no-abbrev" => options.abbrev = Some(0),
            "--candidates" => {
                let Some(value) = iter.next() else {
                    return describe_option_requires_value_error("candidates");
                };
                options.max_candidates = parse_describe_candidates(value)?;
            }
            "--match" => {
                let Some(value) = iter.next() else {
                    return describe_option_requires_value_error("match");
                };
                options.patterns.push(value.clone());
            }
            "--no-match" => options.patterns.clear(),
            "--exclude" => {
                let Some(value) = iter.next() else {
                    return describe_option_requires_value_error("exclude");
                };
                options.exclude_patterns.push(value.clone());
            }
            "--no-exclude" => options.exclude_patterns.clear(),
            value if value.starts_with("--abbrev=") => {
                let raw = &value["--abbrev=".len()..];
                options.abbrev = Some(parse_describe_abbrev(raw)?);
            }
            value if value.starts_with("--candidates=") => {
                options.max_candidates =
                    parse_describe_candidates(&value["--candidates=".len()..])?;
            }
            value if value.starts_with("--dirty=") => {
                options.dirty = Some(value["--dirty=".len()..].to_string());
            }
            value if value.starts_with("--broken=") => {
                options.broken = Some(value["--broken=".len()..].to_string());
            }
            value if value.starts_with("--match=") => {
                options.patterns.push(value["--match=".len()..].to_string());
            }
            value if value.starts_with("--exclude=") => {
                options
                    .exclude_patterns
                    .push(value["--exclude=".len()..].to_string());
            }
            value if value.starts_with("--") => {
                return describe_unknown_option_error(value.trim_start_matches("--"));
            }
            value if value.starts_with('-') && value.len() > 1 => {
                let switch = value.chars().nth(1).unwrap_or('-');
                return describe_unknown_switch_error(switch);
            }
            value => commits.push(value.to_string()),
        }
    }

    if options.contains {
        return Err(GitError::Command(
            "describe --contains is not implemented".into(),
        ));
    }
    if options.long && options.abbrev == Some(0) {
        eprintln!("fatal: options '--long' and '--abbrev=0' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if options.dirty.is_some() && !commits.is_empty() {
        eprintln!("fatal: option '--dirty' and commit-ishes cannot be used together");
        return Err(GitError::Exit(128));
    }
    if options.broken.is_some() && !commits.is_empty() {
        eprintln!("fatal: option '--broken' and commit-ishes cannot be used together");
        return Err(GitError::Exit(128));
    }

    let repo = RepositoryContext::discover_current()?;
    let git_dir = repo.git_dir();
    let format = repo.format();
    let db = repo.objects();

    // Resolve the effective abbreviation length once: an unset/`--abbrev`
    // sentinel falls back to the repository's `core.abbrev` (default 7).
    let abbrev = resolve_describe_abbrev(git_dir, format, options.abbrev)?;

    let tags = collect_describe_tags(git_dir, format, db, &options)?;

    // git refuses to resolve any commit-ish when the (filtered) ref universe is
    // empty and there is no `--always` fallback, reporting that no names exist
    // before even looking at the requested revision. The ref universe includes
    // unannotated tags, so a lightweight-only repo still reaches the walk (which
    // then suggests `--tags`).
    if tags.by_commit.is_empty() && !options.always {
        eprintln!("fatal: No names found, cannot describe anything.");
        return Err(GitError::Exit(128));
    }

    if commits.is_empty() {
        let dirty_suffix = describe_dirty_suffix(git_dir, format, &options)?;
        let head = resolve_describe_commit(&repo, "HEAD")?;
        describe_one(
            format,
            db,
            &options,
            abbrev,
            &tags,
            &head,
            dirty_suffix.as_deref(),
        )
    } else {
        // git describes each commit-ish in order and dies on the first failure,
        // after printing the results of the commits already handled.
        for commit in &commits {
            let target = resolve_describe_commit(&repo, commit)?;
            describe_one(format, db, &options, abbrev, &tags, &target, None)?;
        }
        Ok(())
    }
}

/// Sentinel meaning "user wrote `--abbrev` with no value": use the repo default.
const DEFAULT_ABBREV_SENTINEL: usize = usize::MAX;

struct DescribeOptions {
    all: bool,
    tags: bool,
    long: bool,
    first_parent: bool,
    always: bool,
    debug: bool,
    contains: bool,
    abbrev: Option<usize>,
    max_candidates: usize,
    dirty: Option<String>,
    broken: Option<String>,
    patterns: Vec<String>,
    exclude_patterns: Vec<String>,
}

impl Default for DescribeOptions {
    fn default() -> Self {
        Self {
            all: false,
            tags: false,
            long: false,
            first_parent: false,
            always: false,
            debug: false,
            contains: false,
            abbrev: None,
            max_candidates: DEFAULT_CANDIDATES,
            dirty: None,
            broken: None,
            patterns: Vec::new(),
            exclude_patterns: Vec::new(),
        }
    }
}

/// The single tag chosen to name a given commit. When several tags point at the
/// same commit, git keeps the "best" one: annotated beats lightweight, then the
/// newest date wins, then (via ref iteration order) the lexicographically first
/// name. We capture the comparison keys so collection order does not matter.
struct DescribeTag {
    /// The display name (e.g. `v1.0`, or `tags/v1.0`/`heads/main` under --all).
    name: String,
    /// Priority: annotated tags (2) outrank lightweight tags / plain refs (1).
    prio: u8,
    /// Tagger date (annotated) or committer date (lightweight), for ordering.
    date: i64,
}

impl DescribeTag {
    /// True when `other` should replace `self` as a commit's chosen name, i.e.
    /// `other` is strictly better under git's (prio, date, name) ordering.
    fn outranked_by(&self, other: &DescribeTag) -> bool {
        match other.prio.cmp(&self.prio) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => match other.date.cmp(&self.date) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => other.name < self.name,
            },
        }
    }
}

/// Whether a tag may serve as a describe candidate under the current options.
/// Annotated tags always qualify; unannotated tags/refs need `--tags` or `--all`.
fn describe_eligible(tag: &DescribeTag, options: &DescribeOptions) -> bool {
    tag.prio == 2 || options.tags || options.all
}

/// The candidate ref universe for a describe run: the single best tag naming
/// each commit. Lightweight tags are included even in the default (annotated
/// only) mode — git keeps them in its name map so it can both detect that the
/// ref universe is non-empty and, during the walk, suggest `--tags` when only
/// unannotated tags are reachable. Eligibility is applied later at use sites.
struct DescribeTags {
    by_commit: HashMap<ObjectId, DescribeTag>,
}

/// Gather candidate tags/refs, honouring `--all`/`--match`/`--exclude`,
/// collapsing multiple tags on one commit to the single best per git's rules.
fn collect_describe_tags(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: &DescribeOptions,
) -> Result<DescribeTags> {
    let store = FileRefStore::new(git_dir, format);
    let mut by_commit: HashMap<ObjectId, DescribeTag> = HashMap::new();
    for reference in store.list_refs()? {
        let RefTarget::Direct(oid) = &reference.target else {
            // Symbolic refs (e.g. a packed HEAD) never name a commit directly.
            continue;
        };
        let Some(names) = describe_ref_names(&reference.name, options) else {
            continue;
        };
        if !describe_ref_passes_filters(&names.match_name, options) {
            continue;
        }
        // Priority mirrors git: annotated tag (2) > lightweight tag (1) > any
        // other ref such as a branch under `--all` (0).
        let (commit, prio, date) = match describe_peel_commit(db, format, oid)? {
            DescribePeel::Annotated { commit, date } => (commit, 2, date),
            DescribePeel::Lightweight { commit, date } => {
                (commit, if names.is_tag { 1 } else { 0 }, date)
            }
            DescribePeel::NotACommit => continue,
        };
        let candidate = DescribeTag {
            name: names.display,
            prio,
            date,
        };
        match by_commit.get(&commit) {
            Some(existing) if !existing.outranked_by(&candidate) => {}
            _ => {
                by_commit.insert(commit, candidate);
            }
        }
    }

    Ok(DescribeTags { by_commit })
}

/// The naming of a candidate ref: its display name, the name `--match`/`--exclude`
/// test against, and whether it lives under `refs/tags/`.
struct DescribeRefName {
    display: String,
    match_name: String,
    is_tag: bool,
}

/// Compute the naming for a ref, or `None` if the ref is not eligible given
/// `--all`. Without `--all`, only `refs/tags/*` are considered; `--all` admits
/// every ref, displaying it with the leading `refs/` stripped.
fn describe_ref_names(refname: &str, options: &DescribeOptions) -> Option<DescribeRefName> {
    if let Some(tag) = refname.strip_prefix("refs/tags/") {
        let display = if options.all {
            format!("tags/{tag}")
        } else {
            tag.to_string()
        };
        return Some(DescribeRefName {
            display,
            match_name: tag.to_string(),
            is_tag: true,
        });
    }
    if !options.all {
        return None;
    }
    // `--all` considers every ref; the display name drops the leading `refs/`.
    let display = refname.strip_prefix("refs/").unwrap_or(refname).to_string();
    Some(DescribeRefName {
        match_name: display.clone(),
        display,
        is_tag: false,
    })
}

fn describe_ref_passes_filters(match_name: &str, options: &DescribeOptions) -> bool {
    if !options.patterns.is_empty()
        && !options
            .patterns
            .iter()
            .any(|pattern| refname_pattern_matches(pattern, match_name))
    {
        return false;
    }
    if options
        .exclude_patterns
        .iter()
        .any(|pattern| refname_pattern_matches(pattern, match_name))
    {
        return false;
    }
    true
}

/// Result of peeling a ref toward the commit it names.
enum DescribePeel {
    /// An annotated tag object naming a commit; date is the tagger date.
    Annotated { commit: ObjectId, date: i64 },
    /// A ref pointing straight at a commit (lightweight tag / branch); date is
    /// that commit's committer date.
    Lightweight { commit: ObjectId, date: i64 },
    /// The ref does not resolve to a commit (e.g. a tag of a tree/blob).
    NotACommit,
}

/// Peel a ref target to the commit it names. The returned date is used to order
/// candidates so that, at equal depth, the newest tag wins: the tagger date for
/// annotated tags, otherwise the committer date.
fn describe_peel_commit(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<DescribePeel> {
    let object = db.read_object(oid)?;
    match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse(format, &object.body)?;
            let date = commit_identity_timestamp_i64(&commit.committer).unwrap_or(0);
            Ok(DescribePeel::Lightweight { commit: *oid, date })
        }
        ObjectType::Tag => {
            let tag = Tag::parse(format, &object.body)?;
            // Prefer the tagger date; fall back to the peeled commit's date.
            let commit = sley_rev::peel_to_commit(db, format, &tag.object)?;
            let peeled = db.read_object(&commit)?;
            if peeled.object_type != ObjectType::Commit {
                return Ok(DescribePeel::NotACommit);
            }
            let parsed = Commit::parse(format, &peeled.body)?;
            let date = tag
                .tagger
                .as_deref()
                .and_then(|tagger| commit_identity_timestamp_i64(tagger).ok())
                .or_else(|| commit_identity_timestamp_i64(&parsed.committer).ok())
                .unwrap_or(0);
            Ok(DescribePeel::Annotated { commit, date })
        }
        _ => Ok(DescribePeel::NotACommit),
    }
}

/// State tracked for each candidate tag during the priority walk.
struct PossibleTag<'a> {
    tag: &'a DescribeTag,
    /// Bit identifying commits reachable from this candidate's tagged commit.
    flag: u32,
    /// Count of traversed commits not reachable from this candidate.
    depth: u32,
    /// Order in which this candidate was registered during the commit-date walk;
    /// breaks ties between equal-depth candidates, matching git's `compare_pt`.
    found_order: usize,
}

/// Describe a single target commit, printing the result (or an error per git).
fn describe_one(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: &DescribeOptions,
    abbrev: usize,
    tags: &DescribeTags,
    target: &ObjectId,
    dirty_suffix: Option<&str>,
) -> Result<()> {
    if options.debug {
        eprintln!("describe {}", target.to_hex());
    }

    // Exact match: the target commit is itself named by an eligible tag.
    if let Some(best) = tags
        .by_commit
        .get(target)
        .filter(|tag| describe_eligible(tag, options))
    {
        let suffix = dirty_suffix.unwrap_or("");
        if options.long {
            println!(
                "{}-0-g{}{suffix}",
                best.name,
                describe_abbrev_oid(db, target, abbrev)?
            );
        } else {
            println!("{}{suffix}", best.name);
        }
        return Ok(());
    }

    // Zero candidates (`--exact-match` or `--candidates=0`) means only exact
    // matches are acceptable; without one, git errors even under `--always`.
    if options.max_candidates == 0 {
        eprintln!("fatal: no tag exactly matches '{}'", target.to_hex());
        return Err(GitError::Exit(128));
    }

    if options.debug {
        eprintln!("No exact match on refs or tags, searching to describe");
    }

    let search = describe_search(format, db, options, &tags.by_commit, target)?;

    let Some((best, traversed)) = search.found else {
        // No candidate tag was reachable from the target.
        return describe_no_candidate(
            db,
            options,
            abbrev,
            search.unannotated_cnt,
            target,
            dirty_suffix,
        );
    };

    if options.debug {
        eprintln!("traversed {traversed} commits");
    }

    let suffix = dirty_suffix.unwrap_or("");
    let short = describe_abbrev_oid(db, target, abbrev)?;
    if options.long || best.depth != 0 {
        if abbrev == 0 {
            // `--abbrev=0` without `--long`: print just the tag name.
            println!("{}{suffix}", best.tag.name);
        } else {
            println!("{}-{}-g{short}{suffix}", best.tag.name, best.depth);
        }
    } else {
        println!("{}{suffix}", best.tag.name);
    }
    Ok(())
}

/// Per-candidate flags live in a u32; bit 0 is reserved (git's flags are 1-based
/// via `1u << match_cnt` after the post-increment), so we can track up to 31
/// candidates. git is likewise bounded by its commit-flag bits.
const MAX_FLAG_CANDIDATES: usize = 31;

/// Run the commit-date-ordered priority walk from `target`, returning the winning
/// candidate together with the number of commits traversed. Returns `Ok(None)`
/// when no candidate tag is reachable. This mirrors git's `describe_commit`
/// walk, including the `depth = seen_commits - 1` seeding, the per-commit depth
/// increments, the early-exit, and the `finish_depth_computation` tail.
fn describe_search<'a>(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: &DescribeOptions,
    tags: &'a HashMap<ObjectId, DescribeTag>,
    target: &ObjectId,
) -> Result<DescribeSearchResult<'a>> {
    let mut candidates: Vec<PossibleTag<'a>> = Vec::new();
    // Flags carried by each commit: the union of candidate bits whose tagged
    // commit this commit is an ancestor-or-self of, propagated to parents.
    let mut flags: HashMap<ObjectId, u32> = HashMap::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut queue: std::collections::BinaryHeap<DescribeQueueItem> =
        std::collections::BinaryHeap::new();

    let target_date = describe_commit_date(db, format, target)?;
    queue.push(DescribeQueueItem {
        date: target_date,
        oid: *target,
    });
    seen.insert(*target);

    let effective_max = options.max_candidates.min(MAX_FLAG_CANDIDATES);
    let names_size = tags.len();
    let mut seen_commits = 0usize;
    let mut annotated_cnt = 0usize;
    // Reachable unannotated tags skipped because the default mode wants annotated
    // tags; drives the "try --tags" hint when no annotated tag is reachable.
    let mut unannotated_cnt = 0usize;
    // The commit at which we stopped because the candidate budget was exhausted;
    // it must be re-fed to the depth-finishing pass.
    let mut gave_up: Option<DescribeQueueItem> = None;

    while let Some(item) = queue.pop() {
        let oid = item.oid;
        seen_commits += 1;

        // Stop collecting once the candidate budget (or the entire tag universe)
        // is exhausted; the winner's depth is finished separately below.
        if candidates.len() == effective_max || candidates.len() == names_size {
            gave_up = Some(DescribeQueueItem {
                date: item.date,
                oid,
            });
            seen_commits -= 1;
            break;
        }

        if let Some(best) = tags.get(&oid) {
            if !describe_eligible(best, options) {
                // A reachable unannotated tag we would have used with `--tags`.
                unannotated_cnt += 1;
            } else if candidates.len() < effective_max {
                // git assigns the flag/found_order from the post-incremented
                // match count, making both 1-based.
                let found_order = candidates.len() + 1;
                let flag = 1u32 << found_order;
                let depth = (seen_commits - 1) as u32;
                *flags.entry(oid).or_insert(0) |= flag;
                if options.debug {
                    eprintln!(" annotated {depth:>10} {}", best.name);
                }
                candidates.push(PossibleTag {
                    tag: best,
                    flag,
                    depth,
                    found_order,
                });
                if best.prio == 2 {
                    annotated_cnt += 1;
                }
            }
        }

        // Every candidate not reached by this commit grows its depth by one.
        let commit_flags = flags.get(&oid).copied().unwrap_or(0);
        for candidate in &mut candidates {
            if commit_flags & candidate.flag == 0 {
                candidate.depth += 1;
            }
        }

        // Early exit: if the queue is drained to commits all covered by the best
        // candidate(s), remaining depth is already final.
        if annotated_cnt > 0 && queue.is_empty() {
            let mut best_depth = u32::MAX;
            let mut best_within = 0u32;
            for candidate in &candidates {
                if candidate.depth < best_depth {
                    best_depth = candidate.depth;
                    best_within = candidate.flag;
                } else if candidate.depth == best_depth {
                    best_within |= candidate.flag;
                }
            }
            if commit_flags & best_within == best_within {
                break;
            }
        }

        let parents = describe_commit_parents(db, format, &oid, options.first_parent)?;
        for parent in parents {
            *flags.entry(parent).or_insert(0) |= commit_flags;
            if seen.insert(parent) {
                let date = describe_commit_date(db, format, &parent)?;
                queue.push(DescribeQueueItem { date, oid: parent });
            }
        }
    }

    if candidates.is_empty() {
        return Ok(DescribeSearchResult {
            found: None,
            unannotated_cnt,
        });
    }

    // Pick the winner: smallest depth, ties broken by registration order.
    let mut best_index = 0;
    for index in 1..candidates.len() {
        let challenger = &candidates[index];
        let leader = &candidates[best_index];
        if challenger.depth < leader.depth
            || (challenger.depth == leader.depth && challenger.found_order < leader.found_order)
        {
            best_index = index;
        }
    }

    // Finish the winner's depth over any commits left unprocessed (because the
    // walk stopped early or gave up on the candidate budget).
    let best_flag = candidates[best_index].flag;
    if let Some(gave_up) = gave_up {
        seen.remove(&gave_up.oid);
        queue.push(gave_up);
    }
    let extra = finish_depth_computation(
        format, db, options, &mut queue, &mut flags, &mut seen, best_flag,
    )?;
    candidates[best_index].depth += extra;

    seen_commits += extra as usize;
    let best = candidates.swap_remove(best_index);
    Ok(DescribeSearchResult {
        found: Some((best, seen_commits)),
        unannotated_cnt,
    })
}

/// The outcome of the describe walk: the winning candidate (with the commits
/// traversed) if one was found, plus the count of reachable unannotated tags
/// skipped in the default mode.
struct DescribeSearchResult<'a> {
    found: Option<(PossibleTag<'a>, usize)>,
    unannotated_cnt: usize,
}

/// Continue walking the leftover queue to finish counting the winning
/// candidate's depth, mirroring git's `finish_depth_computation`: every commit
/// not reachable from the winning tag still lies between the target and that
/// tag and adds one to its depth. Returns the additional depth accumulated.
fn finish_depth_computation(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    options: &DescribeOptions,
    queue: &mut std::collections::BinaryHeap<DescribeQueueItem>,
    flags: &mut HashMap<ObjectId, u32>,
    seen: &mut HashSet<ObjectId>,
    best_flag: u32,
) -> Result<u32> {
    // Commits currently queued that the winner does not yet reach.
    let mut unflagged: HashSet<ObjectId> = queue
        .iter()
        .filter(|item| flags.get(&item.oid).copied().unwrap_or(0) & best_flag == 0)
        .map(|item| item.oid)
        .collect();
    let mut extra = 0u32;
    while let Some(item) = queue.pop() {
        let oid = item.oid;
        let commit_flags = flags.get(&oid).copied().unwrap_or(0);
        if commit_flags & best_flag != 0 {
            // The winner reaches this commit; once nothing unflagged remains the
            // depth can no longer grow.
            if unflagged.is_empty() {
                break;
            }
        } else {
            unflagged.remove(&oid);
            extra += 1;
        }
        let parents = describe_commit_parents(db, format, &oid, options.first_parent)?;
        for parent in parents {
            let flag_before = flags.get(&parent).copied().unwrap_or(0) & best_flag;
            let was_seen = seen.contains(&parent);
            if !was_seen {
                seen.insert(parent);
            }
            *flags.entry(parent).or_insert(0) |= commit_flags;
            let flag_after = flags.get(&parent).copied().unwrap_or(0) & best_flag;
            if !was_seen {
                let date = describe_commit_date(db, format, &parent)?;
                queue.push(DescribeQueueItem { date, oid: parent });
                if flag_after == 0 {
                    unflagged.insert(parent);
                }
            } else if flag_before == 0 && flag_after != 0 {
                unflagged.remove(&parent);
            }
        }
    }
    Ok(extra)
}

/// Handle the case where no reachable tag names the target: emit the abbreviated
/// commit under `--always`, otherwise the appropriate fatal error. When some
/// candidate refs exist but none are reachable, git distinguishes the
/// "unannotated tags exist, try --tags" case from the generic "no tags" case.
fn describe_no_candidate(
    db: &FileObjectDatabase,
    options: &DescribeOptions,
    abbrev: usize,
    unannotated_cnt: usize,
    target: &ObjectId,
    dirty_suffix: Option<&str>,
) -> Result<()> {
    if options.always {
        // git's `--always` uses strbuf_add_unique_abbrev, where abbrev==0 means
        // the FULL oid (unlike the `tag-N-g<short>` path where abbrev==0 drops
        // the suffix entirely).
        let short = if abbrev == 0 {
            target.to_hex()
        } else {
            describe_abbrev_oid(db, target, abbrev)?
        };
        println!("{short}{}", dirty_suffix.unwrap_or(""));
        return Ok(());
    }
    let oid = target.to_hex();
    if unannotated_cnt > 0 {
        eprintln!("fatal: No annotated tags can describe '{oid}'.");
        eprintln!("However, there were unannotated tags: try --tags.");
    } else {
        eprintln!("fatal: No tags can describe '{oid}'.");
        eprintln!("Try --always, or create some tags.");
    }
    Err(GitError::Exit(128))
}

/// `--always` still respects an explicit `--abbrev=0` request by widening to the
/// minimum, mirroring git's fallback which never prints a zero-length name.
/// Item for the commit-date priority queue. `Ord` yields a max-heap on date so
/// the newest commit is processed first; ties fall back to oid for determinism.
struct DescribeQueueItem {
    date: i64,
    oid: ObjectId,
}

impl PartialEq for DescribeQueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.date == other.date && self.oid == other.oid
    }
}

impl Eq for DescribeQueueItem {}

impl Ord for DescribeQueueItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.date
            .cmp(&other.date)
            .then_with(|| self.oid.to_hex().cmp(&other.oid.to_hex()))
    }
}

impl PartialOrd for DescribeQueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn describe_commit_date(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<i64> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            oid.to_hex(),
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse(format, &object.body)?;
    Ok(commit_identity_timestamp_i64(&commit.committer).unwrap_or(0))
}

fn describe_commit_parents(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    first_parent: bool,
) -> Result<Vec<ObjectId>> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {}, found {}",
            oid.to_hex(),
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse(format, &object.body)?;
    if first_parent {
        Ok(commit.parents.into_iter().take(1).collect())
    } else {
        Ok(commit.parents)
    }
}

/// Resolve a commit-ish to the commit it names, peeling tags. On any resolution
/// failure git prints `fatal: Not a valid object name <rev>` and exits 128.
fn resolve_describe_commit(repo: &RepositoryContext, rev: &str) -> Result<ObjectId> {
    let oid = match repo.resolve_revision(rev) {
        Ok(oid) => oid,
        Err(GitError::Exit(code)) => return Err(GitError::Exit(code)),
        Err(_) => {
            eprintln!("fatal: Not a valid object name {rev}");
            return Err(GitError::Exit(128));
        }
    };
    let object = repo.objects().read_object(&oid)?;
    match object.object_type {
        ObjectType::Commit => Ok(oid),
        ObjectType::Tag => sley_rev::peel_to_commit(repo.objects(), repo.format(), &oid),
        other => {
            eprintln!("fatal: {} is neither a commit nor blob", oid.to_hex());
            let _ = other;
            Err(GitError::Exit(128))
        }
    }
}

/// Compute the dirty/broken suffix for the implicit HEAD case. `--dirty` appends
/// its mark only when the tracked working tree differs from the index/HEAD;
/// untracked files do not count. Errors computing status fall back to `--broken`.
fn describe_dirty_suffix(
    git_dir: &Path,
    format: ObjectFormat,
    options: &DescribeOptions,
) -> Result<Option<String>> {
    if options.dirty.is_none() && options.broken.is_none() {
        return Ok(None);
    }
    let worktree_root = worktree_root_for_git_dir(git_dir)?;
    let mut dirty = false;
    match sley_worktree::stream_short_status(&worktree_root, git_dir, format, |entry| {
        let index_dirty = entry.index != b' ' && entry.index != b'?' && entry.index != b'!';
        let worktree_dirty =
            entry.worktree != b' ' && entry.worktree != b'?' && entry.worktree != b'!';
        if index_dirty || worktree_dirty {
            dirty = true;
            return Ok(sley_worktree::StreamControl::Stop);
        }
        Ok(sley_worktree::StreamControl::Continue)
    }) {
        Ok(()) => {
            if options.dirty.is_some() && dirty {
                return Ok(options.dirty.clone());
            }
            Ok(None)
        }
        Err(err) => {
            if let Some(mark) = &options.broken {
                Ok(Some(mark.clone()))
            } else {
                Err(err)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Abbreviation
// ---------------------------------------------------------------------------

/// Resolve the requested abbreviation length to a concrete width. `None` and the
/// `--abbrev` sentinel both fall back to `core.abbrev` (default 7). A non-zero
/// width is clamped to at least `MINIMUM_ABBREV` and at most the full hex length.
fn resolve_describe_abbrev(
    git_dir: &Path,
    format: ObjectFormat,
    requested: Option<usize>,
) -> Result<usize> {
    let width = match requested {
        None | Some(DEFAULT_ABBREV_SENTINEL) => repository_abbrev(git_dir, format)?.unwrap_or(0),
        Some(0) => 0,
        Some(width) => width,
    };
    Ok(clamp_describe_abbrev(width, format))
}

fn clamp_describe_abbrev(width: usize, format: ObjectFormat) -> usize {
    if width == 0 {
        0
    } else {
        width.max(MINIMUM_ABBREV).min(format.hex_len())
    }
}

/// Abbreviate a commit object name to at least `width` hex digits, growing the
/// prefix until it uniquely identifies an object in the repository (mirroring
/// git's `find_unique_abbrev`). A `width` of 0 yields an empty string.
fn describe_abbrev_oid(db: &FileObjectDatabase, oid: &ObjectId, width: usize) -> Result<String> {
    let hex = oid.to_hex();
    if width == 0 {
        return Ok(String::new());
    }
    let mut len = width.max(MINIMUM_ABBREV).min(hex.len());
    while len < hex.len() {
        match db.resolve_prefix(&hex[..len]) {
            Ok(ObjectPrefixResolution::Unique(_)) => break,
            Ok(ObjectPrefixResolution::Ambiguous(_)) => len += 1,
            // Missing should not happen for an object we hold; stop widening.
            Ok(ObjectPrefixResolution::Missing) => break,
            Err(_) => break,
        }
    }
    Ok(hex[..len].to_string())
}

// ---------------------------------------------------------------------------
// Argument parsing helpers and errors
// ---------------------------------------------------------------------------

fn parse_describe_abbrev(value: &str) -> Result<usize> {
    // `--abbrev` takes a plain integer (no k/m/g suffix). git treats zero as "no
    // abbreviation" and negative values as a request for the default length.
    match value.parse::<i64>() {
        Ok(0) => Ok(0),
        Ok(parsed) if parsed < 0 => Ok(DEFAULT_ABBREV_SENTINEL),
        Ok(parsed) => Ok(parsed as usize),
        Err(_) => {
            eprintln!("error: option `abbrev' expects a numerical value");
            Err(GitError::Exit(129))
        }
    }
}

fn parse_describe_candidates(value: &str) -> Result<usize> {
    // `--candidates` is a magnitude: a non-negative integer with an optional
    // k/m/g (1024-based) suffix. Negative values clamp to zero (exact-match).
    match parse_describe_magnitude(value) {
        Some(parsed) if parsed < 0 => Ok(0),
        Some(parsed) => Ok(parsed as usize),
        None => {
            eprintln!(
                "error: option `candidates' expects an integer value with an optional k/m/g suffix"
            );
            Err(GitError::Exit(129))
        }
    }
}

/// Parse an integer with an optional `k`/`m`/`g` (1024-based) suffix, saturating
/// rather than overflowing. Returns `None` for non-numeric input.
fn parse_describe_magnitude(value: &str) -> Option<i64> {
    let (digits, multiplier) = match value.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1024_i64),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1),
    };
    let base = digits.parse::<i64>().ok()?;
    Some(base.saturating_mul(multiplier))
}

fn describe_option_requires_value_error(option: &str) -> Result<()> {
    eprintln!("error: option `{option}' requires a value");
    Err(GitError::Exit(129))
}

fn describe_unknown_option_error(option: &str) -> Result<()> {
    eprintln!("error: unknown option `{option}'");
    print_describe_usage(&mut io::stderr());
    Err(GitError::Exit(129))
}

fn describe_unknown_switch_error(switch: char) -> Result<()> {
    eprintln!("error: unknown switch `{switch}'");
    print_describe_usage(&mut io::stderr());
    Err(GitError::Exit(129))
}

fn print_describe_usage(out: &mut impl Write) {
    let _ = out.write_all(
        br#"usage: git describe [--all] [--tags] [--contains] [--abbrev=<n>] [<commit-ish>...]
   or: git describe [--all] [--tags] [--contains] [--abbrev=<n>] --dirty[=<mark>]
   or: git describe <blob>

    --[no-]contains       find the tag that comes after the commit
    --[no-]debug          debug search strategy on stderr
    --[no-]all            use any ref
    --[no-]tags           use any tag, even unannotated
    --[no-]long           always use long format
    --[no-]first-parent   only follow first parent
    --[no-]abbrev[=<n>]   use <n> digits to display object names
    --[no-]exact-match    only output exact matches
    --[no-]candidates <n> consider <n> most recent tags (default: 10)
    --[no-]match <pattern>
                          only consider tags matching <pattern>
    --[no-]exclude <pattern>
                          do not consider tags matching <pattern>
    --[no-]always         show abbreviated commit object as fallback
    --[no-]dirty[=<mark>] append <mark> on dirty working tree (default: "-dirty")
    --[no-]broken[=<mark>]
                          append <mark> on broken working tree (default: "-broken")

"#,
    );
}
