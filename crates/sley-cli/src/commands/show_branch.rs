//! `git show-branch` — show branches and the commits they contain.
//!
//! This renders the classic show-branch matrix: a header block listing each
//! selected branch (one per line, `[name] subject`, prefixed by a per-branch
//! marker column), a `--` separator whose width equals the number of branches,
//! and a body that walks the union of the branches' histories one commit per
//! line. Each body line carries a marker column — one character per branch —
//! where `*` marks a commit on the current branch, `+` a commit on another
//! branch, `-` a merge commit, and a space a commit not reachable from that
//! branch. Each commit is labelled with a name derived from the branch heads
//! (`topic`, `topic^`, `topic~2`, `main^2`, ...).
//!
//! The traversal, stopping rule, commit-naming scheme, marker semantics and
//! option set follow git's `builtin/show-branch.c` so the output is
//! byte-compatible. `--list`/`--more`/`--all`/`--remotes`/`--current`/
//! `--sha1-name`/`--no-name`/`--topics`/`--sparse`/`--merge-base`/
//! `--independent`/`--topo-order`/`--date-order` are supported.
//!
//! Shared CLI helpers (`cli_git_dir`, `repository_object_format`,
//! `read_repo_config`, `resolve_revision`, `FileObjectDatabase`,
//! `FileRefStore`, ...) are brought into scope through a glob of the crate
//! root, the same pattern the other `commands::*` submodules use; a submodule
//! can see its ancestor module's private items, so nothing has to be re-listed.

use crate::*;
use sley::plumbing::sley_rev;

/// git's `REV_SHIFT`: the two low flag bits are reserved (`UNINTERESTING`
/// occupies bit 0), so ref `i` is tracked by bit `i + REV_SHIFT`.
const REV_SHIFT: u32 = 2;

/// git's `UNINTERESTING` flag (bit 0).
const UNINTERESTING: u32 = 1;

/// git's `MAX_REVS = FLAG_BITS - REV_SHIFT`, with `FLAG_BITS = 8 * sizeof(int)`
/// on a 32-bit `int`. show-branch refuses to handle more refs than this.
const MAX_REVS: usize = 32 - REV_SHIFT as usize;

/// Default abbreviation width used for `--sha1-name` and for unnamed commits,
/// matching git's `DEFAULT_ABBREV`.
const DEFAULT_ABBREV: usize = 7;

/// Commit-ordering for the matrix body, mirroring git's `enum rev_sort_order`
/// subset that show-branch uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortOrder {
    /// git's `REV_SORT_IN_GRAPH_ORDER` — the default *and* `--topo-order`.
    Graph,
    /// git's `REV_SORT_BY_COMMIT_DATE` — `--date-order`.
    ByDate,
}

/// What `cmd_show_branch` should ultimately do, selected by the mutually
/// exclusive `--more`/`--list`/`--merge-base`/`--independent` family. The
/// matrix variant carries `extra`, git's `--more=<n>` counter (`-1` for
/// `--list`, `0` for the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Matrix { extra: i64 },
    MergeBase,
    Independent,
}

/// Parsed `git show-branch` invocation.
struct ShowBranchOptions {
    mode: Mode,
    all_heads: bool,
    all_remotes: bool,
    with_current_branch: bool,
    sha1_name: bool,
    no_name: bool,
    topics: bool,
    sparse: bool,
    sort_order: SortOrder,
    /// Literal `<rev>`/`<glob>` arguments, in order.
    revs: Vec<String>,
}

impl Default for ShowBranchOptions {
    fn default() -> Self {
        Self {
            // git initialises `extra = 0`; a bare `--more` sets it to 1 and
            // `--list` to -1.
            mode: Mode::Matrix { extra: 0 },
            all_heads: false,
            all_remotes: false,
            with_current_branch: false,
            sha1_name: false,
            no_name: false,
            topics: false,
            sparse: false,
            sort_order: SortOrder::Graph,
            revs: Vec::new(),
        }
    }
}

/// A selected ref: the display name (`main`, `topic^`, a raw arg, ...) plus the
/// commit it resolves to.
struct SelectedRef {
    name: String,
    oid: ObjectId,
}

/// HEAD's display name (`main`, or `HEAD` when detached) and the commit it
/// points at, used to decide the `*` current-branch marker and `--current`.
struct HeadInfo {
    name: String,
    oid: ObjectId,
}

pub(crate) fn cmd_show_branch(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let options = parse_args(args)?;

    let cwd = env::current_dir()?;
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let refs = FileRefStore::new(git_dir.clone(), format);

    let mut state = TraversalState::new(&db, format);
    run(&options, &git_dir, format, &db, &refs, &mut state)
}

/// Drive the whole command after parsing: collect refs, resolve them to
/// commits, seed the traversal, then dispatch to the requested mode.
fn run(
    options: &ShowBranchOptions,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    refs: &FileRefStore,
    state: &mut TraversalState,
) -> Result<()> {
    // Collect the ref names exactly the way git does: explicit args first (in
    // order), then `--all`/`--remotes` snarfing, then `--current`. With no
    // selection at all, default to all local heads.
    let mut ref_names: Vec<String> = Vec::new();

    let mut all_heads = options.all_heads;
    let mut all_remotes = options.all_remotes;
    // git: `-a/--all` shows local *and* remote-tracking branches.
    if all_heads {
        all_remotes = true;
    }
    // With no revs and neither `--all` nor `--remotes`, default to all heads.
    if options.revs.is_empty() && !all_heads && !all_remotes {
        all_heads = true;
    }

    for rev in &options.revs {
        append_one_rev(git_dir, format, db, refs, rev, &mut ref_names)?;
    }
    if all_heads || all_remotes {
        snarf_refs(refs, all_heads, all_remotes, &mut ref_names)?;
    }

    // Resolve HEAD (its short name and the commit it points at) for `--current`
    // and the `*` current-branch marker.
    let head = head_info(git_dir, format, db, refs)?;
    let head_name = head.as_ref().map(|h| h.name.clone());
    if options.with_current_branch
        && let Some(head) = &head_name
        && !ref_names.iter().any(|name| rev_is_head(head, name))
    {
        append_one_rev(git_dir, format, db, refs, head, &mut ref_names)?;
    }

    // Resolve each ref name to a commit, building the `rev[]` array and seeding
    // per-ref flag bits. Bad refs are fatal (git: "bad sha1 reference").
    let mut selected: Vec<SelectedRef> = Vec::new();
    for name in &ref_names {
        if selected.len() >= MAX_REVS {
            eprintln!("fatal: cannot handle more than {MAX_REVS} revs.");
            return Err(GitError::Exit(128));
        }
        let oid = match resolve_to_commit(git_dir, format, db, refs, name) {
            Some(oid) => oid,
            None => {
                eprintln!("fatal: bad sha1 reference {name}");
                return Err(GitError::Exit(128));
            }
        };
        selected.push(SelectedRef {
            name: name.clone(),
            oid,
        });
    }

    if selected.is_empty() {
        eprintln!("No revs to be shown.");
        return Ok(());
    }

    // Seed flag bits and the priority queue, mirroring git's resolution loop.
    let mut rev_mask = vec![0u32; selected.len()];
    for (i, sref) in selected.iter().enumerate() {
        let flag = 1u32 << (i as u32 + REV_SHIFT);
        state.mark_seen(&sref.oid)?;
        let combined = state.add_flag(&sref.oid, flag)?;
        if combined == flag {
            state.queue_push(&sref.oid)?;
        }
    }
    for (i, sref) in selected.iter().enumerate() {
        rev_mask[i] = state.flags(&sref.oid);
    }

    // git only propagates flags when `extra >= 0` (so `--list`/`more=-1` keeps
    // the seed flags only), then date-sorts `seen` before any mode dispatch.
    let extra = match options.mode {
        Mode::Matrix { extra } => extra,
        // `--merge-base`/`--independent` run with the default `extra` (0) unless
        // combined with --more/--list, which this CLI does not special-case.
        Mode::MergeBase | Mode::Independent => 0,
    };
    if extra >= 0 {
        join_revs(state, selected.len(), extra)?;
    }
    sort_seen_by_date(state)?;

    match options.mode {
        Mode::Independent => show_independent(state, &selected, &rev_mask),
        Mode::MergeBase => show_merge_base(state, selected.len()),
        Mode::Matrix { extra } => show_matrix(
            ShowMatrixContext {
                options,
                selected: &selected,
                head: head.as_ref(),
                commit_line: CommitLineContext {
                    git_dir,
                    format,
                    db,
                    no_name: options.no_name,
                    sha1_name: options.sha1_name,
                },
            },
            state,
            extra,
        ),
    }
}

/// git's `commit_list_sort_by_date(&seen)`: order the seen list newest
/// committer-date first, used directly by `--merge-base` and as the stable
/// `orig` input to the topological sort. The sort is stable, preserving the
/// front-inserted order among equal-dated commits.
fn sort_seen_by_date(state: &mut TraversalState) -> Result<()> {
    let mut keyed: Vec<(i64, usize, ObjectId)> = Vec::with_capacity(state.seen.len());
    for (idx, oid) in state.seen.clone().iter().enumerate() {
        let time = state.committer_time(oid)?;
        keyed.push((time, idx, *oid));
    }
    keyed.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    state.seen = keyed.into_iter().map(|(_, _, oid)| oid).collect();
    Ok(())
}

// ---------------------------------------------------------------------------
// Traversal state
// ---------------------------------------------------------------------------

/// Per-commit bookkeeping shared across the traversal, naming and display
/// phases. Holds the object-flag map (git's `commit->object.flags`), the
/// `seen` insertion order (git's `seen` commit_list, used as the display
/// order before sorting), the priority queue for `join_revs`, the assigned
/// display names, and a parsed-commit cache so each object is read once.
struct TraversalState<'a> {
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    flags: HashMap<ObjectId, u32>,
    /// Insertion order of first-seen commits (front-inserted like git's
    /// `commit_list_insert`, so it is reversed relative to discovery).
    seen: Vec<ObjectId>,
    /// `join_revs` work queue, kept date-sorted on pop.
    queue: Vec<ObjectId>,
    names: HashMap<ObjectId, CommitName>,
    parsed: HashMap<ObjectId, std::rc::Rc<Commit>>,
}

/// A commit's display name: a base branch-derived string plus a first-parent
/// generation count, matching git's `commit_name { head_name, generation }`.
#[derive(Clone)]
struct CommitName {
    head_name: String,
    generation: u32,
}

impl<'a> TraversalState<'a> {
    fn new(db: &'a FileObjectDatabase, format: ObjectFormat) -> Self {
        Self {
            db,
            format,
            flags: HashMap::new(),
            seen: Vec::new(),
            queue: Vec::new(),
            names: HashMap::new(),
            parsed: HashMap::new(),
        }
    }

    /// Read and cache a commit, returning a shared handle so repeated lookups
    /// during traversal/naming/display do not re-parse the object.
    fn commit(&mut self, oid: &ObjectId) -> Result<std::rc::Rc<Commit>> {
        if let Some(commit) = self.parsed.get(oid) {
            return Ok(commit.clone());
        }
        let object = self.db.read_object(oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = std::rc::Rc::new(Commit::parse(self.format, &object.body)?);
        self.parsed.insert(*oid, commit.clone());
        Ok(commit)
    }

    fn parents(&mut self, oid: &ObjectId) -> Result<Vec<ObjectId>> {
        Ok(self.commit(oid)?.parents.clone())
    }

    fn flags(&self, oid: &ObjectId) -> u32 {
        self.flags.get(oid).copied().unwrap_or(0)
    }

    /// git's `mark_seen`: record a commit in the `seen` list the first time its
    /// flags are zero (i.e. on first encounter). Returns whether it was added.
    fn mark_seen(&mut self, oid: &ObjectId) -> Result<bool> {
        if self.flags(oid) == 0 && !self.seen.contains(oid) {
            // git front-inserts into `seen`; preserve that so the pre-sort
            // display order matches.
            self.seen.insert(0, *oid);
            return Ok(true);
        }
        Ok(false)
    }

    /// OR `flag` into a commit's flags, returning the new combined value.
    fn add_flag(&mut self, oid: &ObjectId, flag: u32) -> Result<u32> {
        let entry = self.flags.entry(*oid).or_insert(0);
        *entry |= flag;
        Ok(*entry)
    }

    fn queue_push(&mut self, oid: &ObjectId) -> Result<()> {
        self.queue.push(*oid);
        Ok(())
    }

    /// Pop the newest-by-committer-date commit from the queue, matching git's
    /// date-ordered priority queue. Ties keep insertion order (stable).
    fn queue_pop_newest(&mut self) -> Result<Option<ObjectId>> {
        if self.queue.is_empty() {
            return Ok(None);
        }
        let mut best_idx = 0usize;
        let mut best_time = self.committer_time(&self.queue[0].clone())?;
        for idx in 1..self.queue.len() {
            let time = self.committer_time(&self.queue[idx].clone())?;
            if time > best_time {
                best_time = time;
                best_idx = idx;
            }
        }
        Ok(Some(self.queue.remove(best_idx)))
    }

    /// Whether any queued commit still lacks `UNINTERESTING` (git's
    /// `interesting`).
    fn queue_has_interesting(&self) -> bool {
        self.queue
            .iter()
            .any(|oid| self.flags(oid) & UNINTERESTING == 0)
    }

    /// Committer timestamp (seconds since epoch) for ordering; falls back to 0
    /// when unparsable so traversal still terminates.
    fn committer_time(&mut self, oid: &ObjectId) -> Result<i64> {
        let commit = self.commit(oid)?;
        Ok(committer_seconds(&commit.committer).unwrap_or(0))
    }
}

// ---------------------------------------------------------------------------
// join_revs — propagate ref flags down the history (git's join_revs)
// ---------------------------------------------------------------------------

/// Propagate each ref's reachability flag to ancestors, deciding which commits
/// are "interesting". Mirrors git's `join_revs`:
///
/// - `all_revs` is the mask of every ref bit; a commit reachable from all refs
///   becomes a merge point and is marked `UNINTERESTING`.
/// - We keep going while some queued commit is still interesting, *or* while
///   `extra` (the `--more` budget) has not run out. Each newly seen parent
///   decrements `extra`, so the walk reaches a bounded number of commits past
///   the common ancestor.
fn join_revs(state: &mut TraversalState, num_rev: usize, extra: i64) -> Result<()> {
    let all_mask: u32 = (1u32 << (REV_SHIFT + num_rev as u32)) - 1;
    let all_revs: u32 = all_mask & !((1u32 << REV_SHIFT) - 1);
    let mut extra = extra;

    // git peeks the newest queued commit, marks it seen, propagates its flags to
    // parents (skipping parents that already carry all of them), and only counts
    // a parent against `extra` when it is genuinely new *and* nothing
    // interesting remains. The queue is a date-priority queue, so we pop newest.
    while !state.queue.is_empty() {
        let still_interesting = state.queue_has_interesting();
        // Peek the newest commit (git's prio_queue_peek).
        let Some(commit) = state.queue_pop_newest()? else {
            break;
        };

        if !still_interesting && extra <= 0 {
            // Stop before marking this commit seen, exactly like git's
            // `if (!still_interesting && extra <= 0) break;`.
            break;
        }

        state.mark_seen(&commit)?;
        let mut flags = state.flags(&commit) & all_mask;
        if (flags & all_revs) == all_revs {
            flags |= UNINTERESTING;
        }

        let parents = state.parents(&commit)?;
        for parent in &parents {
            let this_flag = state.flags(parent);
            // Skip parents that already carry every bit we would add.
            if (this_flag & flags) == flags {
                continue;
            }
            let newly_seen = state.mark_seen(parent)?;
            if newly_seen && !still_interesting {
                extra -= 1;
            }
            state.add_flag(parent, flags)?;
            if !state.queue.contains(parent) {
                state.queue_push(parent)?;
            }
        }
    }

    // Postprocess: complete the well-poisoning. Any seen commit that is a merge
    // point or already UNINTERESTING marks its (already-seen) parents
    // UNINTERESTING too, iterating to a fixed point.
    loop {
        let mut changed = false;
        let seen = state.seen.clone();
        for oid in &seen {
            let f = state.flags(oid);
            if (f & all_revs) != all_revs && (f & UNINTERESTING) == 0 {
                continue;
            }
            let parents = state.parents(oid)?;
            for parent in &parents {
                if state.flags(parent) & UNINTERESTING == 0 {
                    state.add_flag(parent, UNINTERESTING)?;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Display: the matrix (header + separator + body)
// ---------------------------------------------------------------------------

/// Borrowed inputs shared across matrix rendering.
struct ShowMatrixContext<'a> {
    options: &'a ShowBranchOptions,
    selected: &'a [SelectedRef],
    head: Option<&'a HeadInfo>,
    commit_line: CommitLineContext<'a>,
}

/// Rendering inputs needed to name a single commit in the matrix body.
#[derive(Clone, Copy)]
struct CommitLineContext<'a> {
    git_dir: &'a Path,
    format: ObjectFormat,
    db: &'a FileObjectDatabase,
    no_name: bool,
    sha1_name: bool,
}

/// Render the show-branch matrix. `extra < 0` (`--list`) prints only the header
/// block and stops; otherwise the separator and the per-commit body follow.
fn show_matrix(
    context: ShowMatrixContext<'_>,
    state: &mut TraversalState,
    extra: i64,
) -> Result<()> {
    let options = context.options;
    let selected = context.selected;
    let head = context.head;
    let num_rev = selected.len();
    let mut stdout = io::stdout();

    // Index of the ref that is the current branch (git's `head_at`), if any.
    let mut head_at: Option<usize> = None;

    // Header: shown when there is more than one rev, or in `--list` mode.
    if num_rev > 1 || extra < 0 {
        for (i, sref) in selected.iter().enumerate() {
            // git's `is_head`: the ref name resolves to HEAD's branch *and* the
            // ref's commit is exactly HEAD's commit.
            let is_head = head
                .map(|h| rev_is_head(&h.name, &sref.name) && h.oid == sref.oid)
                .unwrap_or(false);
            if extra < 0 {
                write!(
                    stdout,
                    "{} [{}] ",
                    if is_head { '*' } else { ' ' },
                    sref.name
                )?;
            } else {
                for _ in 0..i {
                    write!(stdout, " ")?;
                }
                write!(
                    stdout,
                    "{} [{}] ",
                    if is_head { '*' } else { '!' },
                    sref.name
                )?;
            }
            let commit = state.commit(&sref.oid)?;
            writeln!(stdout, "{}", oneline_subject(&commit.message))?;
            if is_head {
                head_at = Some(i);
            }
        }
        if extra >= 0 {
            for _ in 0..num_rev {
                write!(stdout, "-")?;
            }
            writeln!(stdout)?;
        }
    }

    if extra < 0 {
        stdout.flush()?;
        return Ok(());
    }

    // Order the body. git sorts `seen` topologically (with the chosen
    // sub-order) before display.
    let ordered = sort_for_display(state, options.sort_order)?;

    // Assign commit names (skipped for --sha1-name / --no-name).
    if !options.sha1_name && !options.no_name {
        name_commits(state, &ordered, selected)?;
    }

    let all_mask: u32 = (1u32 << (REV_SHIFT + num_rev as u32)) - 1;
    let all_revs: u32 = all_mask & !((1u32 << REV_SHIFT) - 1);

    let mut shown_merge_point = false;
    let mut extra = extra;

    for oid in &ordered {
        let this_flag = state.flags(oid);
        let is_merge_point = (this_flag & all_revs) == all_revs;
        shown_merge_point |= is_merge_point;

        if num_rev > 1 {
            let parents = state.parents(oid)?;
            let is_merge = parents.len() > 1;
            // `--topics`: drop commits that are on the first branch but are not
            // a common merge point.
            if options.topics && !is_merge_point && (this_flag & (1u32 << REV_SHIFT)) != 0 {
                continue;
            }
            // Dense (default) view omits merges reachable from only one tip.
            if !options.sparse && is_merge && omit_in_dense(oid, this_flag, selected) {
                continue;
            }
            for (i, _sref) in selected.iter().enumerate() {
                let mark = if this_flag & (1u32 << (i as u32 + REV_SHIFT)) == 0 {
                    ' '
                } else if is_merge {
                    '-'
                } else if Some(i) == head_at {
                    '*'
                } else {
                    '+'
                };
                write!(stdout, "{mark}")?;
            }
            write!(stdout, " ")?;
        }

        write_one_commit(&mut stdout, &context.commit_line, state, oid)?;

        if shown_merge_point {
            extra -= 1;
            if extra < 0 {
                break;
            }
        }
    }

    stdout.flush()?;
    Ok(())
}

/// git's `omit_in_dense`: a merge is hidden in the dense view when it carries
/// exactly one ref's flag and is not itself one of the named tips.
fn omit_in_dense(oid: &ObjectId, flags: u32, selected: &[SelectedRef]) -> bool {
    if selected.iter().any(|s| &s.oid == oid) {
        return false;
    }
    let count = (0..selected.len())
        .filter(|i| flags & (1u32 << (*i as u32 + REV_SHIFT)) != 0)
        .count();
    count == 1
}

/// Emit a single body line's `[name] subject` (or, with `--no-name`, just the
/// subject). Unnamed commits and `--sha1-name` fall back to an abbreviated oid.
fn write_one_commit(
    stdout: &mut io::Stdout,
    context: &CommitLineContext<'_>,
    state: &mut TraversalState,
    oid: &ObjectId,
) -> Result<()> {
    let commit = state.commit(oid)?;
    let subject = oneline_subject(&commit.message);
    if context.no_name {
        writeln!(stdout, "{subject}")?;
        return Ok(());
    }
    if !context.sha1_name
        && let Some(name) = state.names.get(oid)
    {
        write!(stdout, "[{}", name.head_name)?;
        if name.generation == 1 {
            write!(stdout, "^")?;
        } else if name.generation >= 2 {
            write!(stdout, "~{}", name.generation)?;
        }
        write!(stdout, "] ")?;
    } else {
        let abbrev = unique_abbrev(context.git_dir, context.format, context.db, oid)?;
        write!(stdout, "[{abbrev}] ")?;
    }
    writeln!(stdout, "{subject}")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Display: --merge-base and --independent
// ---------------------------------------------------------------------------

/// git's `show_merge_base`: print every common-ancestor commit's full oid.
/// Exit status 1 when none were printed (no merge base), 0 otherwise.
fn show_merge_base(state: &mut TraversalState, num_rev: usize) -> Result<()> {
    let all_mask: u32 = (1u32 << (REV_SHIFT + num_rev as u32)) - 1;
    let all_revs: u32 = all_mask & !((1u32 << REV_SHIFT) - 1);
    // git iterates the `seen` list directly here (no topological sort).
    let ordered = state.seen.clone();
    let mut stdout = io::stdout();
    let mut found = false;
    for oid in &ordered {
        let flags = state.flags(oid) & all_mask;
        if flags & UNINTERESTING == 0 && (flags & all_revs) == all_revs {
            writeln!(stdout, "{oid}")?;
            found = true;
            state.add_flag(oid, UNINTERESTING)?;
        }
    }
    stdout.flush()?;
    if found {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

/// git's `show_independent`: print each ref whose flags equal only its own seed
/// mask (i.e. it is reachable from no other ref). Always exit 0.
fn show_independent(
    state: &mut TraversalState,
    selected: &[SelectedRef],
    rev_mask: &[u32],
) -> Result<()> {
    let mut stdout = io::stdout();
    for (i, sref) in selected.iter().enumerate() {
        let flag = rev_mask.get(i).copied().unwrap_or(0);
        if state.flags(&sref.oid) == flag {
            writeln!(stdout, "{}", sref.oid)?;
        }
        state.add_flag(&sref.oid, UNINTERESTING)?;
    }
    stdout.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Commit naming (git's name_commits / name_first_parent_chain / name_parent)
// ---------------------------------------------------------------------------

/// Assign each shown commit a display name derived from the branch heads,
/// reproducing git's three-phase `name_commits`:
///
/// 1. name each head commit with its ref name at generation 0;
/// 2. extend names down first-parent chains (`name~k`) until stable;
/// 3. name the remaining (non-first-parent) commits with a `^n` suffix, then
///    extend their first-parent chains, until everything reachable is named.
fn name_commits(
    state: &mut TraversalState,
    ordered: &[ObjectId],
    selected: &[SelectedRef],
) -> Result<()> {
    // Phase 1: heads.
    for oid in ordered {
        if state.names.contains_key(oid) {
            continue;
        }
        if let Some(sref) = selected.iter().find(|s| &s.oid == oid) {
            state.names.insert(
                *oid,
                CommitName {
                    head_name: sref.name.clone(),
                    generation: 0,
                },
            );
        }
    }

    // Phase 2: first-parent chains, repeated until no progress.
    loop {
        let mut progressed = 0usize;
        for oid in ordered {
            progressed += name_first_parent_chain(state, oid)?;
        }
        if progressed == 0 {
            break;
        }
    }

    // Phase 3: remaining commits via `^n` suffixes, then their chains.
    loop {
        let mut progressed = 0usize;
        for oid in ordered {
            let Some(name) = state.names.get(oid).cloned() else {
                continue;
            };
            let parents = state.parents(oid)?;
            for (idx, parent) in parents.iter().enumerate() {
                let nth = idx + 1;
                if state.names.contains_key(parent) {
                    continue;
                }
                let base = match name.generation {
                    0 => name.head_name.clone(),
                    1 => format!("{}^", name.head_name),
                    g => format!("{}~{}", name.head_name, g),
                };
                let new_name = if nth == 1 {
                    format!("{base}^")
                } else {
                    format!("{base}^{nth}")
                };
                state.names.insert(
                    *parent,
                    CommitName {
                        head_name: new_name,
                        generation: 0,
                    },
                );
                progressed += 1;
                name_first_parent_chain(state, parent)?;
            }
        }
        if progressed == 0 {
            break;
        }
    }
    Ok(())
}

/// git's `name_first_parent_chain`: walk first parents from `start`, naming each
/// unnamed parent with the child's `head_name` and `generation + 1`. The walk
/// stops at a root or at the first parent that is *already named* — git breaks
/// there without renaming, so the first name a commit receives is the one it
/// keeps. Returns how many commits it named.
fn name_first_parent_chain(state: &mut TraversalState, start: &ObjectId) -> Result<usize> {
    let mut count = 0usize;
    let mut current = start.clone();
    loop {
        if !state.names.contains_key(&current) {
            break;
        }
        let parents = state.parents(&current)?;
        let Some(parent) = parents.first().cloned() else {
            break;
        };
        if state.names.contains_key(&parent) {
            // Already named: git's `name_first_parent_chain` breaks here, leaving
            // the existing name untouched.
            break;
        }
        let child = match state.names.get(&current) {
            Some(name) => name.clone(),
            None => break,
        };
        state.names.insert(
            parent,
            CommitName {
                head_name: child.head_name,
                generation: child.generation + 1,
            },
        );
        count += 1;
        current = parent;
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Display ordering (git's sort_in_topological_order)
// ---------------------------------------------------------------------------

/// Order the `seen` commits for display, a faithful port of git's
/// `sort_in_topological_order`. The result is always topological (a commit
/// appears before its parents). Two sub-orders are supported, matching git:
///
/// - [`SortOrder::Graph`] (git's `REV_SORT_IN_GRAPH_ORDER`, the default and the
///   `--topo-order` order): ready commits are taken from a LIFO stack that is
///   reversed after seeding, which reproduces git's "graph order".
/// - [`SortOrder::ByDate`] (git's `REV_SORT_BY_COMMIT_DATE`, the `--date-order`
///   order): ready commits are taken newest committer-date first.
///
/// The `seen` list is consumed in its current (front-inserted, i.e. reverse
/// discovery) order, exactly the `orig` list git feeds the sort.
fn sort_for_display(state: &mut TraversalState, order: SortOrder) -> Result<Vec<ObjectId>> {
    let nodes: Vec<ObjectId> = state.seen.clone();
    let node_set: HashSet<ObjectId> = nodes.iter().cloned().collect();

    // git's indegree: every listed commit starts at 1, then +1 for each listed
    // child that has it as a parent. A commit with indegree 1 is a "ready" tip.
    let mut indegree: HashMap<ObjectId, i64> = HashMap::new();
    for oid in &nodes {
        indegree.insert(*oid, 1);
    }
    for oid in &nodes {
        let parents = state.parents(oid)?;
        for parent in parents {
            if let Some(value) = indegree.get_mut(&parent)
                && *value != 0
            {
                *value += 1;
            }
            let _ = &node_set;
        }
    }

    // Seed the work queue with indegree-1 commits, in `nodes` order.
    let mut queue: Vec<ObjectId> = nodes
        .iter()
        .filter(|oid| indegree.get(*oid).copied().unwrap_or(0) == 1)
        .cloned()
        .collect();

    // Graph order reverses the seed queue (git's `prio_queue_reverse`) and then
    // pops LIFO; the two reversals cancel so the initial tips come out in
    // `nodes` order, with newly-ready parents interleaved depth-first.
    if order == SortOrder::Graph {
        queue.reverse();
    }

    let mut out = Vec::with_capacity(nodes.len());
    while !queue.is_empty() {
        let oid = match order {
            // LIFO pop.
            SortOrder::Graph => queue.remove(queue.len() - 1),
            // Newest committer date first; stable on insertion order for ties.
            SortOrder::ByDate => {
                let mut best = 0usize;
                let mut best_time: Option<i64> = None;
                for (idx, oid) in queue.clone().iter().enumerate() {
                    let time = state.committer_time(oid)?;
                    if best_time.is_none_or(|bt| time > bt) {
                        best_time = Some(time);
                        best = idx;
                    }
                }
                queue.remove(best)
            }
        };
        let parents = state.parents(&oid)?;
        for parent in parents {
            if let Some(value) = indegree.get_mut(&parent) {
                if *value == 0 {
                    continue;
                }
                *value -= 1;
                if *value == 1 {
                    queue.push(parent);
                }
            }
        }
        if let Some(value) = indegree.get_mut(&oid) {
            *value = 0;
        }
        out.push(oid);
    }

    // A well-formed DAG emits everything; guard against losing rows regardless.
    if out.len() != nodes.len() {
        for oid in &nodes {
            if !out.contains(oid) {
                out.push(*oid);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Ref selection helpers
// ---------------------------------------------------------------------------

/// git's `append_one_rev`: a literal rev resolves directly; otherwise a value
/// containing glob metacharacters is matched against refs; anything else is a
/// fatal "bad sha1 reference". Returns the names appended (already deduped).
fn append_one_rev(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    refs: &FileRefStore,
    rev: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    if resolve_to_commit(git_dir, format, db, refs, rev).is_some() {
        append_ref(rev.to_string(), out);
        return Ok(());
    }
    if rev.contains('*') || rev.contains('?') || rev.contains('[') {
        let before = out.len();
        append_matching_refs(refs, rev, out)?;
        if out.len() == before && out.len() < MAX_REVS {
            eprintln!("error: no matching refs with {rev}");
        }
        return Ok(());
    }
    eprintln!("fatal: bad sha1 reference {rev}");
    Err(GitError::Exit(128))
}

/// git's `append_ref`: append a ref name unless it is already present (no
/// duplicates) and the cap is not exceeded.
fn append_ref(name: String, out: &mut Vec<String>) {
    if out.iter().any(|existing| existing == &name) {
        return;
    }
    if out.len() >= MAX_REVS {
        eprintln!("warning: ignoring {name}; cannot handle more than {MAX_REVS} refs");
        return;
    }
    out.push(name);
}

/// git's `snarf_refs`: append all `refs/heads/*` (when `heads`) then all
/// `refs/remotes/*` (when `remotes`), each group sorted by name, in their short
/// form.
fn snarf_refs(
    refs: &FileRefStore,
    heads: bool,
    remotes: bool,
    out: &mut Vec<String>,
) -> Result<()> {
    let all = refs.list_refs()?;
    if heads {
        let mut names: Vec<String> = all
            .iter()
            .filter_map(|r| r.name.strip_prefix("refs/heads/").map(str::to_string))
            .collect();
        names.sort();
        for name in names {
            append_ref(name, out);
        }
    }
    if remotes {
        let mut names: Vec<String> = all
            .iter()
            .filter_map(|r| r.name.strip_prefix("refs/remotes/").map(str::to_string))
            .collect();
        names.sort();
        for name in names {
            append_ref(name, out);
        }
    }
    Ok(())
}

/// git's `append_matching_ref`: match a glob against refs/heads, refs/remotes
/// and refs/tags, appending the short head/remote name or the full tag ref, in
/// sorted order. The glob is matched against the ref tail per git's
/// slash-aware `append_matching_ref`, approximated here by matching the short
/// name and the full ref name.
fn append_matching_refs(refs: &FileRefStore, pattern: &str, out: &mut Vec<String>) -> Result<()> {
    let all = refs.list_refs()?;
    let mut matched: Vec<String> = Vec::new();
    for r in &all {
        let candidates: [Option<&str>; 3] = [
            r.name.strip_prefix("refs/heads/"),
            r.name.strip_prefix("refs/remotes/"),
            r.name.strip_prefix("refs/tags/"),
        ];
        let short = candidates.iter().flatten().next().copied();
        let tail = short.unwrap_or(r.name.as_str());
        if wildmatch(pattern, tail) {
            // heads/remotes use the short form; tags use the full ref name.
            let name = if r.name.starts_with("refs/heads/") || r.name.starts_with("refs/remotes/") {
                tail.to_string()
            } else {
                r.name.clone()
            };
            matched.push(name);
        }
    }
    matched.sort();
    for name in matched {
        append_ref(name, out);
    }
    Ok(())
}

/// git's `rev_is_head`: compare HEAD's short branch name to a ref name, after
/// stripping `refs/heads/`/`heads/` from the ref.
fn rev_is_head(head: &str, name: &str) -> bool {
    let head = head.strip_prefix("refs/heads/").unwrap_or(head);
    let name = name
        .strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("heads/"))
        .unwrap_or(name);
    head == name
}

/// Resolve HEAD into the name git uses for the `*` marker / `--current` plus the
/// commit it points at. The name is the short branch name (e.g. `main`) when
/// HEAD is on a branch, or the literal `HEAD` when HEAD is detached at a valid
/// commit. Returns `None` when HEAD is unborn or does not resolve to a commit.
/// This mirrors git's `refs_resolve_refdup(HEAD)` followed by
/// `skip_prefix(head, "refs/heads/", &name)`.
fn head_info(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    refs: &FileRefStore,
) -> Result<Option<HeadInfo>> {
    let name = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => name
            .strip_prefix("refs/heads/")
            .unwrap_or(&name)
            .to_string(),
        Some(RefTarget::Direct(_)) => "HEAD".to_string(),
        None => return Ok(None),
    };
    match resolve_to_commit(git_dir, format, db, refs, "HEAD") {
        Some(oid) => Ok(Some(HeadInfo { name, oid })),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Resolution and formatting helpers
// ---------------------------------------------------------------------------

/// Resolve a revision string to the commit it names (peeling tags), or `None`
/// if it does not resolve or does not name a commit-ish.
///
/// Plain ref names are looked up through the full `gitrevisions` search path
/// (`<name>`, `refs/<name>`, `refs/tags/<name>`, `refs/heads/<name>`,
/// `refs/remotes/<name>`, `refs/remotes/<name>/HEAD`) so that remote-tracking
/// names like `origin/main` resolve the way `git show-branch -r`/`--all`
/// expects. Anything with revision syntax (suffixes, `@{...}`, raw oids, ...)
/// falls back to the shared `resolve_revision` machinery.
fn resolve_to_commit(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    refs: &FileRefStore,
    rev: &str,
) -> Option<ObjectId> {
    if let Some(oid) = resolve_ref_by_search_path(refs, rev) {
        return sley_rev::peel_to_commit(db, format, &oid).ok();
    }
    let oid = resolve_revision(git_dir, format, rev).ok()?;
    sley_rev::peel_to_commit(db, format, &oid).ok()
}

/// Look up a plain ref name via git's ref search path, returning the resolved
/// (symref-followed) object id. Returns `None` for names that contain revision
/// syntax or that no ref matches.
fn resolve_ref_by_search_path(refs: &FileRefStore, rev: &str) -> Option<ObjectId> {
    // Skip anything that is not a bare ref name: revision operators, reflog
    // selectors, path specs, and full oids are handled by `resolve_revision`.
    if rev.is_empty() || rev.contains(['^', '~', ':', '@', '*', '?', '[']) || rev.starts_with('-') {
        return None;
    }
    let candidates = if rev == "HEAD" {
        vec!["HEAD".to_string()]
    } else if rev.starts_with("refs/") {
        vec![rev.to_string()]
    } else {
        vec![
            rev.to_string(),
            format!("refs/{rev}"),
            format!("refs/tags/{rev}"),
            format!("refs/heads/{rev}"),
            format!("refs/remotes/{rev}"),
            format!("refs/remotes/{rev}/HEAD"),
        ]
    };
    for name in candidates {
        if let Ok(Some(target)) = refs.read_ref(&name) {
            return follow_ref_target(refs, target);
        }
    }
    None
}

/// Follow a (possibly symbolic) ref target to a concrete object id.
fn follow_ref_target(refs: &FileRefStore, target: RefTarget) -> Option<ObjectId> {
    match target {
        RefTarget::Direct(oid) => Some(oid),
        RefTarget::Symbolic(name) => {
            let next = refs.read_ref(&name).ok()??;
            follow_ref_target(refs, next)
        }
    }
}

/// The first line of a commit message, with a leading `[PATCH] ` stripped, the
/// way git's `CMIT_FMT_ONELINE` renders a subject for show-branch.
fn oneline_subject(message: &[u8]) -> String {
    let subject = commit_subject(message);
    subject
        .strip_prefix("[PATCH] ")
        .map(str::to_string)
        .unwrap_or(subject)
}

/// Committer timestamp (seconds since epoch) parsed from a `committer` line of
/// the form `Name <email> <seconds> <tz>`.
fn committer_seconds(committer: &[u8]) -> Option<i64> {
    let line = std::str::from_utf8(committer).ok()?;
    let mut fields = line.rsplit(' ');
    let _tz = fields.next()?;
    fields.next()?.parse::<i64>().ok()
}

/// Shortest-unique abbreviation of `oid` (minimum [`DEFAULT_ABBREV`]), used for
/// unnamed commits and `--sha1-name`, matching git's `find_unique_abbrev`.
fn unique_abbrev(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    oid: &ObjectId,
) -> Result<String> {
    let hex = oid.to_hex();
    let configured = repository_abbrev(git_dir, format)?.unwrap_or(DEFAULT_ABBREV);
    let mut width = configured.clamp(DEFAULT_ABBREV.min(hex.len()), hex.len());
    // Extend until the prefix uniquely identifies one object.
    while width < hex.len() {
        match db.resolve_prefix(&hex[..width]) {
            Ok(ObjectPrefixResolution::Unique(_)) => break,
            Ok(ObjectPrefixResolution::Ambiguous(_)) => width += 1,
            _ => break,
        }
    }
    Ok(hex[..width.min(hex.len())].to_string())
}

/// Minimal shell-style glob matcher (`*`, `?`, `[...]`) sufficient for
/// show-branch ref patterns. Matches the whole string.
fn wildmatch(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    wildmatch_inner(&p, &t)
}

fn wildmatch_inner(pattern: &[char], text: &[char]) -> bool {
    let mut pi = 0usize;
    let mut ti = 0usize;
    // Backtracking positions for the most recent `*`.
    let mut star: Option<(usize, usize)> = None;
    while ti < text.len() {
        if pi < pattern.len() {
            match pattern[pi] {
                '*' => {
                    star = Some((pi, ti));
                    pi += 1;
                    continue;
                }
                '?' => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                '[' => {
                    if let Some((consumed, matched)) = match_class(&pattern[pi..], text[ti]) {
                        if matched {
                            pi += consumed;
                            ti += 1;
                            continue;
                        }
                    } else if pattern[pi] == text[ti] {
                        // Malformed class: treat `[` literally.
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
                c if c == text[ti] => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        }
        // Mismatch: backtrack to the last `*` if any.
        if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    // Consume trailing `*`s.
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Match a `[...]` character class at the start of `pattern` against `ch`.
/// Returns `(chars_consumed, matched)` or `None` if the class is unterminated.
fn match_class(pattern: &[char], ch: char) -> Option<(usize, bool)> {
    // pattern[0] == '['
    let mut idx = 1usize;
    let mut negate = false;
    if idx < pattern.len() && (pattern[idx] == '!' || pattern[idx] == '^') {
        negate = true;
        idx += 1;
    }
    let mut matched = false;
    let mut first = true;
    while idx < pattern.len() {
        let c = pattern[idx];
        if c == ']' && !first {
            return Some((idx + 1, matched ^ negate));
        }
        first = false;
        // Range a-b.
        if idx + 2 < pattern.len() && pattern[idx + 1] == '-' && pattern[idx + 2] != ']' {
            let lo = c;
            let hi = pattern[idx + 2];
            if lo <= ch && ch <= hi {
                matched = true;
            }
            idx += 3;
        } else {
            if c == ch {
                matched = true;
            }
            idx += 1;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Parse `git show-branch` arguments. `-h`/`--help` prints git's usage to
/// stdout and exits 129; an unknown option prints `error: ...` plus the usage to
/// stderr and exits 129; recognised options populate [`ShowBranchOptions`].
fn parse_args(args: &[String]) -> Result<ShowBranchOptions> {
    let mut options = ShowBranchOptions::default();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            options.revs.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-h" | "--help" => return Err(print_help()),
            "-a" | "--all" => options.all_heads = true,
            "--no-all" => options.all_heads = false,
            "-r" | "--remotes" => options.all_remotes = true,
            "--no-remotes" => options.all_remotes = false,
            "--current" => options.with_current_branch = true,
            "--no-current" => options.with_current_branch = false,
            "--sha1-name" => options.sha1_name = true,
            "--no-sha1-name" => options.sha1_name = false,
            "--no-name" => options.no_name = true,
            "--name" => options.no_name = false,
            "--topics" => options.topics = true,
            "--no-topics" => options.topics = false,
            "--sparse" => options.sparse = true,
            "--no-sparse" => options.sparse = false,
            "--topo-order" => options.sort_order = SortOrder::Graph,
            "--date-order" => options.sort_order = SortOrder::ByDate,
            "--list" => options.mode = Mode::Matrix { extra: -1 },
            // git accepts `--no-list`/`--no-more` as resets to the default view.
            "--no-list" => options.mode = Mode::Matrix { extra: 0 },
            "--more" => options.mode = Mode::Matrix { extra: 1 },
            "--no-more" => options.mode = Mode::Matrix { extra: 0 },
            "--merge-base" => options.mode = Mode::MergeBase,
            "--no-merge-base" => options.mode = Mode::Matrix { extra: 0 },
            "--independent" => options.mode = Mode::Independent,
            "--no-independent" => options.mode = Mode::Matrix { extra: 0 },
            // Color is accepted but inert: this CLI runs to a pipe and git's
            // default there is no color.
            "--color" | "--no-color" => {}
            value if value.starts_with("--color=") => {}
            value if let Some(rest) = value.strip_prefix("--more=") => {
                options.mode = Mode::Matrix {
                    extra: parse_more(rest)?,
                };
            }
            // `-g`/`--reflog` is not modelled; reject clearly rather than
            // mis-rendering.
            "-g" | "--reflog" => {
                return Err(GitError::Unsupported(
                    "show-branch --reflog is not supported".into(),
                ));
            }
            value if value.starts_with("--reflog=") || value.starts_with("-g") => {
                return Err(GitError::Unsupported(
                    "show-branch --reflog is not supported".into(),
                ));
            }
            value if value.starts_with("--") => return Err(unknown_option(&value[2..])),
            // A bare `-` is treated as a rev/path; other single-dash clusters
            // are unknown options.
            value if value.starts_with('-') && value != "-" => {
                return Err(unknown_option(value.trim_start_matches('-')));
            }
            value => options.revs.push(value.to_string()),
        }
    }
    Ok(options)
}

/// Parse a `--more=<n>` value (a signed integer). On a non-integer git prints a
/// fixed `error: option 'more' expects an integer ...` and exits 129; match that
/// exactly rather than falling through to the generic error path.
fn parse_more(value: &str) -> Result<i64> {
    value.parse::<i64>().map_err(|_| {
        eprintln!("error: option `more' expects an integer value with an optional k/m/g suffix");
        GitError::Exit(129)
    })
}

/// `-h`/`--help`: git prints the usage block to *stdout* and exits 129.
fn print_help() -> GitError {
    print!("{SHOW_BRANCH_USAGE}");
    GitError::Exit(129)
}

/// Emit git's `error: unknown option ...` line followed by the usage text, both
/// to *stderr*, and return the exit-129 sentinel.
fn unknown_option(name: &str) -> GitError {
    eprintln!("error: unknown option `{name}'");
    usage_error()
}

/// Print git's exact `git show-branch` usage block to stderr and return the
/// exit-129 sentinel (the error path: unknown options and parse errors).
fn usage_error() -> GitError {
    eprint!("{SHOW_BRANCH_USAGE}");
    GitError::Exit(129)
}

/// git's verbatim usage text for `git show-branch`.
const SHOW_BRANCH_USAGE: &str =
    "usage: git show-branch [-a | --all] [-r | --remotes] [--topo-order | --date-order]
                       [--current] [--color[=<when>] | --no-color] [--sparse]
                       [--more=<n> | --list | --independent | --merge-base]
                       [--no-name | --sha1-name] [--topics]
                       [(<rev> | <glob>)...]
   or: git show-branch (-g | --reflog)[=<n>[,<base>]] [--list] [<ref>]

    -a, --[no-]all        show remote-tracking and local branches
    -r, --[no-]remotes    show remote-tracking branches
    --[no-]color[=<when>] color '*!+-' corresponding to the branch
    --[no-]more[=<n>]     show <n> more commits after the common ancestor
    --[no-]list           synonym to more=-1
    --no-name             suppress naming strings
    --name                opposite of --no-name
    --[no-]current        include the current branch
    --[no-]sha1-name      name commits with their object names
    --[no-]merge-base     show possible merge bases
    --[no-]independent    show refs unreachable from any other ref
    --topo-order          show commits in topological order
    --[no-]topics         show only commits not on the first branch
    --[no-]sparse         show merges reachable from only one tip
    --date-order          topologically sort, maintaining date order where possible
    -g, --reflog[=<n>[,<base>]]
                          show <n> most recent ref-log entries starting at base

";
