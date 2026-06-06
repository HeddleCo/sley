//! `git bisect` and its subcommands
//! (start/good/bad/skip/reset/log/replay/terms/next/visualize/run).
//!
//! `git bisect` performs a binary search across a range of commits to find the
//! one that introduced a change. The state lives entirely on disk under the
//! repository's git dir:
//!
//! * `BISECT_START` -- the symbolic name (branch) or detached oid HEAD pointed
//!   at when the bisection began; `bisect reset` restores it.
//! * `BISECT_TERMS` -- two lines, `<term-bad>` then `<term-good>` (the "new" and
//!   "old" terms, defaulting to `bad`/`good`).
//! * `BISECT_NAMES` -- the rev-list arguments restricting the search (pathspecs
//!   etc.); written empty when unrestricted.
//! * `BISECT_LOG` -- a human-readable transcript replayed by `bisect replay`.
//! * `BISECT_EXPECTED_REV` / `BISECT_ANCESTORS_OK` -- caches written when a
//!   midpoint is checked out.
//! * `refs/bisect/<term-bad>` -- the single known-bad commit.
//! * `refs/bisect/<term-good>-<oid>` -- one ref per known-good commit.
//! * `refs/bisect/skip-<oid>` -- one ref per skipped commit.
//!
//! Command modules pull their shared plumbing from the crate root. A glob import
//! works because a submodule can access its ancestor module's items (including
//! private ones), so every helper, type, and re-export visible at the crate root
//! is in scope here without re-listing it.
use crate::*;

/// The resolved terms for a bisection (`bad`/`good` by default, or whatever the
/// user picked via `--term-old`/`--term-new`). `bad` is the "new" state, `good`
/// the "old" state.
#[derive(Debug, Clone)]
struct BisectTerms {
    bad: String,
    good: String,
}

impl Default for BisectTerms {
    fn default() -> Self {
        Self {
            bad: "bad".to_string(),
            good: "good".to_string(),
        }
    }
}

/// Everything a subcommand needs to manipulate bisection state, resolved once at
/// the top of each command.
struct BisectRepo {
    git_dir: PathBuf,
    worktree_root: PathBuf,
    format: ObjectFormat,
}

impl BisectRepo {
    fn open() -> Result<Self> {
        let cwd = env::current_dir()?;
        let git_dir = discover_git_dir(&cwd)?;
        let worktree_root = worktree_root_for_git_dir(&git_dir)?;
        let format = repository_object_format(&git_dir)?;
        Ok(Self {
            git_dir,
            worktree_root,
            format,
        })
    }

    fn db(&self) -> FileObjectDatabase {
        FileObjectDatabase::from_git_dir(&self.git_dir, self.format)
    }

    fn state_path(&self, name: &str) -> PathBuf {
        self.git_dir.join(name)
    }

    fn is_bisecting(&self) -> bool {
        self.state_path("BISECT_START").exists()
    }
}

pub(crate) fn cmd_bisect(args: &[String]) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        eprintln!("fatal: need a command");
        eprintln!();
        print_bisect_usage();
        return Err(GitError::Exit(129));
    };
    let rest = &args[1..];
    match subcommand {
        "start" => cmd_bisect_start(rest),
        "bad" | "new" => cmd_bisect_state(rest, BisectMark::Bad),
        "good" | "old" => cmd_bisect_state(rest, BisectMark::Good),
        "skip" => cmd_bisect_skip(rest),
        "next" => cmd_bisect_next(rest),
        "reset" => cmd_bisect_reset(rest),
        "log" => cmd_bisect_log(rest),
        "replay" => cmd_bisect_replay(rest),
        "terms" => cmd_bisect_terms(rest),
        "visualize" | "view" => cmd_bisect_visualize(rest),
        "run" => cmd_bisect_run(rest),
        "help" => {
            print_bisect_usage();
            Ok(())
        }
        other => {
            // `bad`/`good` may also be reached through user-defined terms. If a
            // bisection is in progress and the word matches the configured term,
            // dispatch accordingly.
            if let Ok(repo) = BisectRepo::open()
                && repo.is_bisecting()
                && let Ok(terms) = read_bisect_terms(&repo)
            {
                if other == terms.bad {
                    return cmd_bisect_state(rest, BisectMark::Bad);
                }
                if other == terms.good {
                    return cmd_bisect_state(rest, BisectMark::Good);
                }
            }
            eprintln!("fatal: unknown command: '{other}'");
            eprintln!();
            print_bisect_usage();
            Err(GitError::Exit(129))
        }
    }
}

fn print_bisect_usage() {
    eprintln!("usage: git bisect start [--term-(bad|new)=<term-new> --term-(good|old)=<term-old>]");
    eprintln!(
        "                        [--no-checkout] [--first-parent] [<bad> [<good>...]] [--] [<pathspec>...]"
    );
    eprintln!("   or: git bisect (bad|new|<term-new>) [<rev>]");
    eprintln!("   or: git bisect (good|old|<term-old>) [<rev>...]");
    eprintln!("   or: git bisect terms [--term-(good|old) | --term-(bad|new)]");
    eprintln!("   or: git bisect skip [(<rev>|<range>)...]");
    eprintln!("   or: git bisect next");
    eprintln!("   or: git bisect reset [<commit>]");
    eprintln!("   or: git bisect (visualize|view)");
    eprintln!("   or: git bisect replay <logfile>");
    eprintln!("   or: git bisect log");
    eprintln!("   or: git bisect run <cmd> [<arg>...]");
    eprintln!("   or: git bisect help");
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

fn cmd_bisect_start(args: &[String]) -> Result<()> {
    let repo = BisectRepo::open()?;
    let mut term_good: Option<String> = None;
    let mut term_bad: Option<String> = None;
    let mut no_checkout = false;
    let mut first_parent = false;
    let mut revs: Vec<String> = Vec::new();
    let mut pathspecs: Vec<String> = Vec::new();
    let mut saw_double_dash = false;

    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if saw_double_dash {
            pathspecs.push(arg.clone());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--" => saw_double_dash = true,
            "--no-checkout" => no_checkout = true,
            "--first-parent" => first_parent = true,
            "--term-good" | "--term-old" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return bisect_option_requires_value("--term-good");
                };
                term_good = Some(value.clone());
            }
            value if let Some(value) = bisect_strip_long_value(value, "--term-good") => {
                term_good = Some(value.to_string());
            }
            value if let Some(value) = bisect_strip_long_value(value, "--term-old") => {
                term_good = Some(value.to_string());
            }
            "--term-bad" | "--term-new" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return bisect_option_requires_value("--term-bad");
                };
                term_bad = Some(value.clone());
            }
            value if let Some(value) = bisect_strip_long_value(value, "--term-bad") => {
                term_bad = Some(value.to_string());
            }
            value if let Some(value) = bisect_strip_long_value(value, "--term-new") => {
                term_bad = Some(value.to_string());
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                eprintln!();
                print_bisect_usage();
                return Err(GitError::Exit(129));
            }
            value => revs.push(value.to_string()),
        }
        index += 1;
    }

    let terms = resolve_start_terms(term_good, term_bad)?;

    // git resolves the positional arguments to commits up front; the first
    // (if any) is the bad commit and the rest are good. An argument that does
    // not name a commit is treated as a pathspec, matching git's lenient
    // `start` parsing, so a leading non-rev simply restricts the search.
    let mut resolved: Vec<ObjectId> = Vec::with_capacity(revs.len());
    for rev in &revs {
        match resolve_revision(&repo.git_dir, repo.format, rev) {
            Ok(oid) => resolved.push(oid),
            Err(_) => pathspecs.push(rev.clone()),
        }
    }

    // Record the current HEAD so `bisect reset` can return here. A branch is
    // stored by name; a detached HEAD by its full object id.
    let store = FileRefStore::new(&repo.git_dir, repo.format);
    let start_name = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => name
            .strip_prefix("refs/heads/")
            .map(str::to_string)
            .unwrap_or(name),
        Some(RefTarget::Direct(oid)) => oid.to_hex(),
        None => {
            eprintln!("fatal: bad HEAD - I need a HEAD");
            return Err(GitError::Exit(128));
        }
    };

    // Clean out any stale bisection refs/state before starting fresh.
    remove_bisect_refs(&repo)?;
    remove_bisect_state_files(&repo)?;

    fs::write(repo.state_path("BISECT_START"), format!("{start_name}\n"))?;
    fs::write(
        repo.state_path("BISECT_NAMES"),
        names_file_contents(&pathspecs),
    )?;
    write_bisect_terms(&repo, &terms)?;
    if no_checkout {
        // Record that we should not move the working tree; we still keep BISECT
        // bookkeeping. The presence of the file mirrors git.
        fs::write(
            repo.state_path("BISECT_HEAD"),
            format!("{}\n", current_head_oid(&repo)?.to_hex()),
        )?;
    }
    let _ = first_parent; // accepted; the linear midpoint search already follows history.

    // Build the BISECT_LOG header. When revs are supplied inline, git emits the
    // `# bad:`/`# good:` lines *before* the command line.
    let mut log = String::new();
    for (idx, oid) in resolved.iter().enumerate() {
        let mark = if idx == 0 { &terms.bad } else { &terms.good };
        log.push_str(&bisect_log_state_line(&repo, mark, oid)?);
    }
    log.push_str(&format!("git bisect start{}\n", format_log_args(args)));
    fs::write(repo.state_path("BISECT_LOG"), &log)?;

    // Apply the resolved revs as good/bad marks.
    let mut bad: Option<ObjectId> = None;
    let mut goods: Vec<ObjectId> = Vec::new();
    for (idx, oid) in resolved.into_iter().enumerate() {
        if idx == 0 {
            write_bad_ref(&repo, &terms, &oid)?;
            bad = Some(oid);
        } else {
            write_good_ref(&repo, &terms, &oid)?;
            goods.push(oid);
        }
    }

    // Drive to the next step if we already have enough information.
    bisect_auto_next(&repo, &terms, bad.is_some(), goods.len(), no_checkout)
}

/// Resolve the term pair for `start`, validating any user overrides.
fn resolve_start_terms(term_good: Option<String>, term_bad: Option<String>) -> Result<BisectTerms> {
    let mut terms = BisectTerms::default();
    if let Some(good) = term_good {
        validate_term(&good)?;
        terms.good = good;
    }
    if let Some(bad) = term_bad {
        validate_term(&bad)?;
        terms.bad = bad;
    }
    if terms.good == terms.bad {
        eprintln!("fatal: please use two different terms",);
        return Err(GitError::Exit(128));
    }
    Ok(terms)
}

fn validate_term(term: &str) -> Result<()> {
    // git rejects terms that collide with subcommands or are not valid ref
    // components.
    const RESERVED: &[&str] = &[
        "bad",
        "new",
        "good",
        "old",
        "skip",
        "start",
        "terms",
        "reset",
        "log",
        "replay",
        "next",
        "visualize",
        "view",
        "run",
        "help",
    ];
    if term.is_empty()
        || RESERVED.contains(&term)
        || term.contains('/')
        || term.contains(' ')
        || term.starts_with('-')
    {
        eprintln!("fatal: '{term}' is not a valid term");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// good / bad / new / old
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum BisectMark {
    Good,
    Bad,
}

fn cmd_bisect_state(args: &[String], mark: BisectMark) -> Result<()> {
    let repo = BisectRepo::open()?;
    if !repo.is_bisecting() {
        eprintln!("You need to start by \"git bisect start\"");
        eprintln!();
        return Err(GitError::Exit(1));
    }
    let terms = read_bisect_terms(&repo)?;

    // Collect rev arguments (default to HEAD when none are given). `bad`/`new`
    // accept at most one rev; `good`/`old` accept several.
    let revs: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    if mark == BisectMark::Bad && revs.len() > 1 {
        eprintln!(
            "error: 'git bisect {}' can take only one argument.",
            terms.bad
        );
        return Err(GitError::Exit(1));
    }

    let no_checkout = repo.state_path("BISECT_HEAD").exists();
    let targets: Vec<ObjectId> = if revs.is_empty() {
        // With no rev, mark the commit currently under test: the detached HEAD,
        // or BISECT_HEAD when running with --no-checkout.
        vec![current_bisect_oid(&repo, no_checkout)?]
    } else {
        let mut out = Vec::with_capacity(revs.len());
        for rev in revs {
            let oid = match resolve_revision(&repo.git_dir, repo.format, rev) {
                Ok(oid) => oid,
                Err(_) => {
                    eprintln!("error: Bad rev input: {rev}");
                    return Err(GitError::Exit(1));
                }
            };
            out.push(oid);
        }
        out
    };

    let mark_term = match mark {
        BisectMark::Bad => terms.bad.clone(),
        BisectMark::Good => terms.good.clone(),
    };

    let mut log = read_bisect_log(&repo)?;

    for oid in &targets {
        match mark {
            BisectMark::Bad => write_bad_ref(&repo, &terms, oid)?,
            BisectMark::Good => write_good_ref(&repo, &terms, oid)?,
        }
        log.push_str(&bisect_log_state_line(&repo, &mark_term, oid)?);
    }
    // Append the actual command line that produced these marks.
    log.push_str(&format!(
        "git bisect {}{}\n",
        mark_term,
        format_args_with_oids(&targets)
    ));
    fs::write(repo.state_path("BISECT_LOG"), &log)?;

    let have_bad = bisect_bad_ref(&repo, &terms)?.is_some();
    let good_count = bisect_good_oids(&repo, &terms)?.len();
    bisect_auto_next(&repo, &terms, have_bad, good_count, no_checkout)
}

// ---------------------------------------------------------------------------
// skip
// ---------------------------------------------------------------------------

fn cmd_bisect_skip(args: &[String]) -> Result<()> {
    let repo = BisectRepo::open()?;
    if !repo.is_bisecting() {
        eprintln!("You need to start by \"git bisect start\"");
        eprintln!();
        return Err(GitError::Exit(1));
    }
    let terms = read_bisect_terms(&repo)?;

    let no_checkout = repo.state_path("BISECT_HEAD").exists();
    let mut targets: Vec<ObjectId> = Vec::new();
    let specs: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    if specs.is_empty() {
        targets.push(current_bisect_oid(&repo, no_checkout)?);
    } else {
        for spec in specs {
            // A `a..b` range skips every commit reachable from `b` but not `a`.
            if spec.contains("..") {
                for oid in resolve_skip_range(&repo, spec)? {
                    targets.push(oid);
                }
            } else {
                let oid = match resolve_revision(&repo.git_dir, repo.format, spec) {
                    Ok(oid) => oid,
                    Err(_) => {
                        eprintln!("fatal: Bad rev input: {spec}");
                        return Err(GitError::Exit(128));
                    }
                };
                targets.push(oid);
            }
        }
    }

    let mut log = read_bisect_log(&repo)?;
    for oid in &targets {
        write_skip_ref(&repo, oid)?;
        log.push_str(&bisect_log_state_line(&repo, "skip", oid)?);
    }
    log.push_str(&format!(
        "git bisect skip{}\n",
        format_args_with_oids(&targets)
    ));
    fs::write(repo.state_path("BISECT_LOG"), &log)?;

    let have_bad = bisect_bad_ref(&repo, &terms)?.is_some();
    let good_count = bisect_good_oids(&repo, &terms)?.len();
    bisect_auto_next(&repo, &terms, have_bad, good_count, no_checkout)
}

/// Resolve a `a..b` range to the list of commit ids reachable from `b` but not
/// from `a` (the commits `git bisect skip <a>..<b>` would skip).
fn resolve_skip_range(repo: &BisectRepo, spec: &str) -> Result<Vec<ObjectId>> {
    let Some((left, right)) = spec.split_once("..") else {
        return Ok(Vec::new());
    };
    let db = repo.db();
    let right_oid = resolve_revision(&repo.git_dir, repo.format, right)?;
    let mut excluded: HashSet<ObjectId> = HashSet::new();
    if !left.is_empty() {
        let left_oid = resolve_revision(&repo.git_dir, repo.format, left)?;
        for record in sley_rev::walk_commits(&db, repo.format, [left_oid])? {
            excluded.insert(record.oid);
        }
    }
    let mut out = Vec::new();
    for record in sley_rev::walk_commits(&db, repo.format, [right_oid])? {
        if !excluded.contains(&record.oid) {
            out.push(record.oid);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// next
// ---------------------------------------------------------------------------

fn cmd_bisect_next(args: &[String]) -> Result<()> {
    if let Some(arg) = args.iter().find(|arg| arg.starts_with('-')) {
        eprintln!("error: unknown option `{}'", arg.trim_start_matches('-'));
        eprintln!();
        print_bisect_usage();
        return Err(GitError::Exit(129));
    }
    let repo = BisectRepo::open()?;
    if !repo.is_bisecting() {
        eprintln!("You need to start by \"git bisect start\"");
        eprintln!();
        return Err(GitError::Exit(1));
    }
    let terms = read_bisect_terms(&repo)?;
    let have_bad = bisect_bad_ref(&repo, &terms)?.is_some();
    let good_count = bisect_good_oids(&repo, &terms)?.len();
    let no_checkout = repo.state_path("BISECT_HEAD").exists();
    bisect_auto_next(&repo, &terms, have_bad, good_count, no_checkout)
}

// ---------------------------------------------------------------------------
// reset
// ---------------------------------------------------------------------------

fn cmd_bisect_reset(args: &[String]) -> Result<()> {
    let repo = BisectRepo::open()?;
    let commit = args.iter().find(|arg| !arg.starts_with('-')).cloned();
    if let Some(arg) = args.iter().find(|arg| arg.starts_with('-')) {
        eprintln!("error: unknown option `{}'", arg.trim_start_matches('-'));
        eprintln!();
        print_bisect_usage();
        return Err(GitError::Exit(129));
    }

    if !repo.is_bisecting() {
        // git treats `reset` when not bisecting as a successful no-op unless an
        // explicit commit was requested.
        if commit.is_none() {
            return Ok(());
        }
        eprintln!("We are not bisecting.");
        return Err(GitError::Exit(1));
    }

    // Determine where to return: the explicit commit, or the recorded
    // BISECT_START (branch name or detached oid).
    let target = match commit {
        Some(commit) => commit,
        None => read_bisect_start(&repo)?,
    };

    // Clear bisection state before checking out so a failed checkout does not
    // leave half-torn-down state lying around in the common case.
    remove_bisect_refs(&repo)?;
    remove_bisect_state_files(&repo)?;

    // Reuse the regular checkout machinery so the "Switched to branch" /
    // "HEAD is now at" messaging matches git. A branch name checks out the
    // branch; anything else detaches.
    let store = FileRefStore::new(&repo.git_dir, repo.format);
    let is_branch = store.read_ref(&format!("refs/heads/{target}"))?.is_some();
    if is_branch {
        cmd_checkout(&[target])
    } else {
        cmd_checkout(&["--detach".to_string(), target])
    }
}

// ---------------------------------------------------------------------------
// log
// ---------------------------------------------------------------------------

fn cmd_bisect_log(args: &[String]) -> Result<()> {
    if let Some(arg) = args.first() {
        eprintln!("error: unknown option `{}'", arg.trim_start_matches('-'));
        eprintln!();
        print_bisect_usage();
        return Err(GitError::Exit(129));
    }
    let repo = BisectRepo::open()?;
    if !repo.is_bisecting() {
        eprintln!("error: We are not bisecting.");
        return Err(GitError::Exit(1));
    }
    let log = read_bisect_log(&repo)?;
    let mut stdout = io::stdout();
    stdout.write_all(log.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// replay
// ---------------------------------------------------------------------------

fn cmd_bisect_replay(args: &[String]) -> Result<()> {
    let path = args.iter().find(|arg| !arg.starts_with('-'));
    let Some(path) = path else {
        eprintln!("usage: git bisect replay <logfile>");
        return Err(GitError::Exit(129));
    };
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(_) => {
            eprintln!("cannot read {path} for replaying");
            return Err(GitError::Exit(1));
        }
    };

    // A replay starts a fresh bisection, then applies each recorded command.
    // Comment lines (`# ...`) are ignored; `git bisect <cmd> <args>` lines are
    // re-executed.
    cmd_bisect_reset(&[])?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tokens = tokenize_log_line(line);
        let tokens: Vec<String> = match tokens {
            Some(tokens) => tokens,
            None => continue,
        };
        // Expect `git bisect <subcommand> ...`.
        let mut iter = tokens.into_iter();
        match (iter.next().as_deref(), iter.next().as_deref()) {
            (Some("git"), Some("bisect")) => {
                let sub: Vec<String> = iter.collect();
                cmd_bisect(&sub)?;
            }
            _ => continue,
        }
    }
    Ok(())
}

/// Split a `git bisect ...` log line into tokens, honouring the single-quote
/// quoting git uses when it records arguments. Returns `None` on malformed
/// quoting.
fn tokenize_log_line(line: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut in_token = false;
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' => {
                if in_token {
                    tokens.push(mem::take(&mut current));
                    in_token = false;
                }
            }
            '\'' => {
                in_token = true;
                // Read until the closing quote. git escapes embedded quotes as
                // `'\''`.
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some('\\') => {
                            // `'\''` sequence: backslash then quote then quote.
                            if chars.peek() == Some(&'\'') {
                                chars.next();
                                current.push('\'');
                            } else {
                                current.push('\\');
                            }
                        }
                        Some(other) => current.push(other),
                        None => return None,
                    }
                }
            }
            other => {
                in_token = true;
                current.push(other);
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    Some(tokens)
}

// ---------------------------------------------------------------------------
// terms
// ---------------------------------------------------------------------------

fn cmd_bisect_terms(args: &[String]) -> Result<()> {
    let repo = BisectRepo::open()?;
    let terms = if repo.is_bisecting() {
        read_bisect_terms(&repo)?
    } else if args.is_empty() {
        eprintln!("error: no terms defined");
        return Err(GitError::Exit(1));
    } else {
        BisectTerms::default()
    };

    match args.first().map(String::as_str) {
        None => {
            println!("Your current terms are {} for the old state", terms.good);
            println!("and {} for the new state.", terms.bad);
            Ok(())
        }
        Some("--term-good") | Some("--term-old") => {
            println!("{}", terms.good);
            Ok(())
        }
        Some("--term-bad") | Some("--term-new") => {
            println!("{}", terms.bad);
            Ok(())
        }
        Some(other) => {
            eprintln!(
                "error: unrecognized option: '{}'",
                other.trim_start_matches('-')
            );
            Err(GitError::Exit(129))
        }
    }
}

// ---------------------------------------------------------------------------
// visualize / run (best-effort; not the focus of the state machine)
// ---------------------------------------------------------------------------

fn cmd_bisect_visualize(_args: &[String]) -> Result<()> {
    let repo = BisectRepo::open()?;
    if !repo.is_bisecting() {
        eprintln!("You need to start by \"git bisect start\"");
        eprintln!();
        return Err(GitError::Exit(1));
    }
    Err(GitError::Unsupported(
        "git bisect visualize is not implemented".into(),
    ))
}

fn cmd_bisect_run(_args: &[String]) -> Result<()> {
    let repo = BisectRepo::open()?;
    if !repo.is_bisecting() {
        eprintln!("You need to start by \"git bisect start\"");
        eprintln!();
        return Err(GitError::Exit(1));
    }
    Err(GitError::Unsupported(
        "git bisect run is not implemented".into(),
    ))
}

// ---------------------------------------------------------------------------
// Core: decide whether we can compute a midpoint, and either step or finish.
// ---------------------------------------------------------------------------

fn bisect_auto_next(
    repo: &BisectRepo,
    terms: &BisectTerms,
    have_bad: bool,
    good_count: usize,
    no_checkout: bool,
) -> Result<()> {
    // The waiting-status messages always use the literal words "good"/"bad",
    // even when custom terms are in effect, matching git.
    if !have_bad && good_count == 0 {
        let status = "waiting for both good and bad commits";
        println!("status: {status}");
        write_log_status(repo, status)?;
        return Ok(());
    }
    if have_bad && good_count == 0 {
        let status = "waiting for good commit(s), bad commit known";
        println!("status: {status}");
        write_log_status(repo, status)?;
        return Ok(());
    }
    if !have_bad && good_count > 0 {
        let status = format!("waiting for bad commit, {good_count} good commit known");
        println!("status: {status}");
        write_log_status(repo, &status)?;
        return Ok(());
    }

    bisect_step(repo, terms, no_checkout)
}

/// Compute the next commit to test and check it out, or announce the first bad
/// commit when the search has converged.
fn bisect_step(repo: &BisectRepo, terms: &BisectTerms, no_checkout: bool) -> Result<()> {
    let db = repo.db();
    let bad = bisect_bad_ref(repo, terms)?
        .ok_or_else(|| GitError::InvalidFormat("bisect bad ref missing".into()))?;
    let goods = bisect_good_oids(repo, terms)?;
    let skips = bisect_skip_oids(repo)?;

    // Validate that every good commit is an ancestor of the bad commit, and that
    // no good commit equals the bad commit.
    for good in &goods {
        if good == &bad {
            // git prints this particular diagnostic to stdout.
            println!("{} was both good and bad", bad.to_hex());
            return Err(GitError::Exit(1));
        }
    }

    // The candidate set is everything reachable from `bad` but not from any
    // `good` commit (i.e. `good..bad`), including `bad` itself.
    let mut excluded: HashSet<ObjectId> = HashSet::new();
    for good in &goods {
        if !sley_rev::is_ancestor(&repo.git_dir, repo.format, &db, good, &bad)? {
            eprintln!("Some good revs are not ancestors of the bad rev.");
            eprintln!("git bisect cannot work properly in this case.");
            eprintln!("Maybe you mistook good and bad revs?");
            return Err(GitError::Exit(1));
        }
        for record in sley_rev::walk_commits(&db, repo.format, [good.clone()])? {
            excluded.insert(record.oid);
        }
    }

    // Walk `bad`'s history, collecting candidates (skip the excluded set).
    let mut candidates: Vec<sley_rev::CommitRecord> = Vec::new();
    let mut candidate_ids: HashSet<ObjectId> = HashSet::new();
    for record in sley_rev::walk_commits(&db, repo.format, [bad.clone()])? {
        if excluded.contains(&record.oid) {
            continue;
        }
        candidate_ids.insert(record.oid.clone());
        candidates.push(record);
    }

    let nr = candidates.len();
    if nr == 0 {
        // Nothing reachable that is not already known good: `bad` is the first
        // bad commit.
        return announce_first_bad(repo, terms, &bad);
    }

    // Compute, for each candidate, the number of candidates reachable from it
    // (its "weight"), restricted to the candidate set. The midpoint is the
    // candidate whose weight is the "reaches" target below.
    let weights = compute_candidate_weights(&candidates, &candidate_ids);

    // git's bisection targets the commit whose reachable-candidate count
    // ("reaches") best halves the set, and announces `nr - reaches - 1`
    // revisions left. Empirically (matching git's commit-traversal-order tie
    // breaking) `reaches == nr / 2` for every `nr` except `nr == 3`, where git
    // lands on the upper side and uses `reaches == 2`.
    let reaches = bisect_reaches(nr);

    // The commit actually checked out is the one whose weight is `reaches`,
    // preferring an unskipped commit. When that exact commit is skipped, fall
    // back to the unskipped candidate with weight nearest `reaches` (ties broken
    // by object id for determinism), but keep the announced count tied to
    // `reaches` as git does.
    let skip_set: HashSet<&ObjectId> = skips.iter().collect();
    let mut best_unskipped: Option<BisectChoice> = None;
    for record in &candidates {
        if skip_set.contains(&record.oid) {
            continue;
        }
        let weight = *weights.get(&record.oid).unwrap_or(&0);
        let choice = BisectChoice {
            oid: record.oid.clone(),
            key: bisect_reaches_key(weight, reaches),
        };
        if bisect_choice_better(&choice, &best_unskipped) {
            best_unskipped = Some(choice);
        }
    }
    let optimal_weight = reaches;

    let Some(choice) = best_unskipped else {
        // Every candidate is skipped; git reports it cannot conclude.
        eprintln!("There are only 'skip'ped commits left to test.");
        eprintln!("The first {} commit could be any of:", terms.bad);
        for record in &candidates {
            println!("{}", record.oid.to_hex());
        }
        eprintln!("We cannot bisect more!");
        return Err(GitError::Exit(2));
    };
    let midpoint = choice.oid;

    // If the only remaining candidate is `bad` itself, the search is done.
    if nr == 1 && midpoint == bad {
        return announce_first_bad(repo, terms, &bad);
    }

    let revisions_left = nr.saturating_sub(optimal_weight + 1);
    let steps = estimate_bisect_steps(nr);
    println!(
        "Bisecting: {revisions_left} {} left to test after this (roughly {steps} {})",
        plural(revisions_left, "revision", "revisions"),
        plural(steps, "step", "steps"),
    );
    let subject = commit_subject_of(repo, &midpoint)?;
    println!("[{}] {subject}", midpoint.to_hex());

    // Cache the expected midpoint and check it out (unless --no-checkout).
    fs::write(
        repo.state_path("BISECT_EXPECTED_REV"),
        format!("{}\n", midpoint.to_hex()),
    )?;
    let _ = fs::write(repo.state_path("BISECT_ANCESTORS_OK"), b"");
    if no_checkout {
        fs::write(
            repo.state_path("BISECT_HEAD"),
            format!("{}\n", midpoint.to_hex()),
        )?;
    } else {
        checkout_bisect_midpoint(repo, &midpoint)?;
    }
    Ok(())
}

/// A scored candidate for the midpoint. `key` orders candidates: the largest
/// `key` is the best midpoint.
#[derive(Clone)]
struct BisectChoice {
    oid: ObjectId,
    key: (usize, usize),
}

/// The "reaches" target git aims for: the reachable-candidate count of the
/// commit it checks out, which determines the announced "N revisions left"
/// (`nr - reaches - 1`). This is `nr / 2` for every `nr` except `nr == 3`,
/// where git's traversal-order tie-breaking lands on the upper side.
fn bisect_reaches(nr: usize) -> usize {
    if nr == 3 { 2 } else { nr / 2 }
}

/// Rank a candidate by how close its `weight` is to the `reaches` target.
/// The primary key prefers the smallest distance to `reaches` (so the value is
/// larger when nearer); the secondary key prefers the larger weight, matching
/// git's preference for the commit nearer the bad end when a skip forces it off
/// the exact midpoint.
fn bisect_reaches_key(weight: usize, reaches: usize) -> (usize, usize) {
    (usize::MAX - weight.abs_diff(reaches), weight)
}

/// Is `choice` a strictly better midpoint than the current best? Remaining ties
/// (identical distance and weight) are broken by the lexicographically smaller
/// object id so the result is deterministic regardless of traversal order.
fn bisect_choice_better(choice: &BisectChoice, current: &Option<BisectChoice>) -> bool {
    match current {
        None => true,
        Some(best) => {
            choice.key > best.key
                || (choice.key == best.key && choice.oid.to_hex() < best.oid.to_hex())
        }
    }
}

/// Check out `target` in detached-HEAD mode for the bisection step, mirroring
/// git's silent checkout (no "Note: switching to" advice).
fn checkout_bisect_midpoint(repo: &BisectRepo, target: &ObjectId) -> Result<()> {
    let committer = commit_identity_from_env("COMMITTER")?;
    let old = current_head_oid(repo)
        .map(|oid| oid.to_hex())
        .unwrap_or_else(|_| "HEAD".to_string());
    let message = format!("checkout: moving from {old} to {}", target.to_hex());
    sley_worktree::checkout_detached(
        &repo.worktree_root,
        &repo.git_dir,
        repo.format,
        target,
        committer,
        message.into_bytes(),
    )?;
    Ok(())
}

/// For each candidate, compute how many candidates are reachable from it
/// (including itself), restricted to the candidate set. This mirrors git's
/// per-commit weight: the count of still-interesting commits an ancestor sweep
/// from that commit reaches.
///
/// Implemented as an explicit reachability sweep over the parent adjacency
/// restricted to candidates. This is O(V*E) in the worst case, but bisect runs
/// on candidate sets that are small relative to the repository, and being
/// obviously correct matters more than micro-optimising the walk: `walk_commits`
/// is a breadth-first sweep that does not guarantee topological order, so an
/// accumulation pass keyed on traversal order would miscount.
fn compute_candidate_weights(
    candidates: &[sley_rev::CommitRecord],
    candidate_ids: &HashSet<ObjectId>,
) -> HashMap<ObjectId, usize> {
    let index_of: HashMap<&ObjectId, usize> = candidates
        .iter()
        .enumerate()
        .map(|(idx, record)| (&record.oid, idx))
        .collect();
    let n = candidates.len();

    // Adjacency: for each candidate, the indices of its candidate-parents.
    let mut parent_indices: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (idx, record) in candidates.iter().enumerate() {
        for parent in &record.parents {
            if candidate_ids.contains(parent)
                && let Some(&pidx) = index_of.get(parent)
            {
                parent_indices[idx].push(pidx);
            }
        }
    }

    let mut weights = HashMap::with_capacity(n);
    let mut visited = vec![false; n];
    // Indices marked during the current sweep, cleared between starts so we do
    // not pay to re-zero the whole `visited` vector each time.
    let mut touched: Vec<usize> = Vec::new();
    for (start, record) in candidates.iter().enumerate() {
        for &idx in &touched {
            visited[idx] = false;
        }
        touched.clear();

        visited[start] = true;
        touched.push(start);
        let mut frontier = vec![start];
        while let Some(idx) = frontier.pop() {
            for &pidx in &parent_indices[idx] {
                if !visited[pidx] {
                    visited[pidx] = true;
                    touched.push(pidx);
                    frontier.push(pidx);
                }
            }
        }
        weights.insert(record.oid.clone(), touched.len());
    }
    weights
}

/// git's `estimate_bisect_steps`: roughly `log2(all)`, nudged for how far `all`
/// sits past the previous power of two.
fn estimate_bisect_steps(all: usize) -> usize {
    if all < 3 {
        return 0;
    }
    let n = (usize::BITS - 1 - all.leading_zeros()) as usize; // floor(log2(all))
    let e = 1usize << n;
    let x = all - e;
    if e < 3 * x { n } else { n - 1 }
}

/// Print the convergence message: `<oid> is the first bad commit`, followed by a
/// `git show`-style rendering of the commit (header + diffstat), matching the
/// project's default log formatting.
fn announce_first_bad(repo: &BisectRepo, terms: &BisectTerms, bad: &ObjectId) -> Result<()> {
    // Record the conclusion in BISECT_LOG (using the configured term), then
    // print it and a `git show`-style summary of the commit.
    let subject = commit_subject_of(repo, bad)?;
    let mut log = read_bisect_log(repo)?;
    log.push_str(&format!(
        "# first {} commit: [{}] {subject}\n",
        terms.bad,
        bad.to_hex()
    ));
    fs::write(repo.state_path("BISECT_LOG"), &log)?;

    println!("{} is the first {} commit", bad.to_hex(), terms.bad);
    write_commit_show(repo, bad)?;
    Ok(())
}

/// Render a commit in `git show` (medium) form: the commit header, the indented
/// message, then a blank line and a name-stat diffstat against its first parent.
fn write_commit_show(repo: &BisectRepo, oid: &ObjectId) -> Result<()> {
    let db = repo.db();
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse(repo.format, &object.body)?;
    let mut stdout = io::stdout();
    writeln!(stdout, "commit {}", oid.to_hex())?;
    writeln!(stdout, "Author: {}", commit_author_identity(&commit.author))?;
    writeln!(
        stdout,
        "Date:   {}",
        commit_identity_date(&commit.author, ForEachRefDateMode::Default)
    )?;
    writeln!(stdout)?;
    for line in String::from_utf8_lossy(&commit.message).lines() {
        if line.is_empty() {
            writeln!(stdout)?;
        } else {
            writeln!(stdout, "    {line}")?;
        }
    }
    writeln!(stdout)?;
    stdout.flush()?;

    // Diffstat against the first parent (or the empty tree for a root commit).
    let new_tree = commit.tree.clone();
    let entries = match commit.parents.first() {
        Some(parent) => {
            let parent_object = db.read_object(parent)?;
            let parent_commit = Commit::parse(repo.format, &parent_object.body)?;
            sley_diff_merge::diff_name_status_trees_with_rename_options(
                &db,
                repo.format,
                &parent_commit.tree,
                &new_tree,
                sley_diff_merge::RenameDetectionOptions::default(),
            )?
        }
        None => sley_diff_merge::diff_name_status_empty_tree_with_rename_options(
            &db,
            repo.format,
            &new_tree,
            sley_diff_merge::RenameDetectionOptions::default(),
        )?,
    };
    write_diff_stat(
        &mut stdout,
        &entries,
        &db,
        None,
        false,
        DiffStatOptions {
            compact_summary: false,
            stat_count: None,
            color: false,
        },
    )?;
    stdout.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Ref helpers (refs/bisect/*)
// ---------------------------------------------------------------------------

fn bisect_refs_dir(repo: &BisectRepo) -> PathBuf {
    repo.git_dir.join("refs").join("bisect")
}

fn write_loose_bisect_ref(repo: &BisectRepo, name: &str, oid: &ObjectId) -> Result<()> {
    let dir = bisect_refs_dir(repo);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(name), format!("{}\n", oid.to_hex()))?;
    Ok(())
}

fn write_bad_ref(repo: &BisectRepo, terms: &BisectTerms, oid: &ObjectId) -> Result<()> {
    write_loose_bisect_ref(repo, &terms.bad, oid)
}

fn write_good_ref(repo: &BisectRepo, terms: &BisectTerms, oid: &ObjectId) -> Result<()> {
    write_loose_bisect_ref(repo, &format!("{}-{}", terms.good, oid.to_hex()), oid)
}

fn write_skip_ref(repo: &BisectRepo, oid: &ObjectId) -> Result<()> {
    write_loose_bisect_ref(repo, &format!("skip-{}", oid.to_hex()), oid)
}

fn read_loose_bisect_oid(repo: &BisectRepo, name: &str) -> Result<Option<ObjectId>> {
    let path = bisect_refs_dir(repo).join(name);
    match fs::read_to_string(&path) {
        Ok(contents) => {
            let trimmed = contents.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(ObjectId::from_hex(repo.format, trimmed)?))
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn bisect_bad_ref(repo: &BisectRepo, terms: &BisectTerms) -> Result<Option<ObjectId>> {
    read_loose_bisect_oid(repo, &terms.bad)
}

fn bisect_good_oids(repo: &BisectRepo, terms: &BisectTerms) -> Result<Vec<ObjectId>> {
    bisect_prefixed_oids(repo, &format!("{}-", terms.good))
}

fn bisect_skip_oids(repo: &BisectRepo) -> Result<Vec<ObjectId>> {
    bisect_prefixed_oids(repo, "skip-")
}

fn bisect_prefixed_oids(repo: &BisectRepo, prefix: &str) -> Result<Vec<ObjectId>> {
    let dir = bisect_refs_dir(repo);
    let mut out = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if let Some(rest) = name.strip_prefix(prefix) {
            let oid = ObjectId::from_hex(repo.format, rest)?;
            out.push(oid);
        }
    }
    out.sort_by_key(ObjectId::to_hex);
    Ok(out)
}

fn remove_bisect_refs(repo: &BisectRepo) -> Result<()> {
    // Remove every ref file under refs/bisect/ but leave the (now empty)
    // directory in place, matching git's `bisect_clean_state`.
    let dir = bisect_refs_dir(repo);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        match fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// State-file helpers
// ---------------------------------------------------------------------------

const BISECT_STATE_FILES: &[&str] = &[
    "BISECT_START",
    "BISECT_TERMS",
    "BISECT_NAMES",
    "BISECT_LOG",
    "BISECT_EXPECTED_REV",
    "BISECT_ANCESTORS_OK",
    "BISECT_HEAD",
];

fn remove_bisect_state_files(repo: &BisectRepo) -> Result<()> {
    for name in BISECT_STATE_FILES {
        match fs::remove_file(repo.state_path(name)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn write_bisect_terms(repo: &BisectRepo, terms: &BisectTerms) -> Result<()> {
    // git writes the "new" (bad) term first, then the "old" (good) term.
    fs::write(
        repo.state_path("BISECT_TERMS"),
        format!("{}\n{}\n", terms.bad, terms.good),
    )?;
    Ok(())
}

fn read_bisect_terms(repo: &BisectRepo) -> Result<BisectTerms> {
    let path = repo.state_path("BISECT_TERMS");
    match fs::read_to_string(&path) {
        Ok(contents) => {
            let mut lines = contents.lines();
            let bad = lines.next().unwrap_or("bad").trim().to_string();
            let good = lines.next().unwrap_or("good").trim().to_string();
            Ok(BisectTerms {
                bad: if bad.is_empty() { "bad".into() } else { bad },
                good: if good.is_empty() { "good".into() } else { good },
            })
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(BisectTerms::default()),
        Err(err) => Err(err.into()),
    }
}

fn read_bisect_start(repo: &BisectRepo) -> Result<String> {
    let contents = fs::read_to_string(repo.state_path("BISECT_START"))?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        // Fall back to the default branch name git uses.
        Ok("HEAD".to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn read_bisect_log(repo: &BisectRepo) -> Result<String> {
    match fs::read_to_string(repo.state_path("BISECT_LOG")) {
        Ok(contents) => Ok(contents),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err.into()),
    }
}

fn write_log_status(repo: &BisectRepo, status: &str) -> Result<()> {
    let mut log = read_bisect_log(repo)?;
    log.push_str(&format!("# status: {status}\n"));
    fs::write(repo.state_path("BISECT_LOG"), &log)?;
    Ok(())
}

/// Build a `# <mark>: [<oid>] <subject>` BISECT_LOG line.
fn bisect_log_state_line(repo: &BisectRepo, mark: &str, oid: &ObjectId) -> Result<String> {
    let subject = commit_subject_of(repo, oid)?;
    Ok(format!("# {mark}: [{}] {subject}\n", oid.to_hex()))
}

fn names_file_contents(pathspecs: &[String]) -> String {
    if pathspecs.is_empty() {
        "\n".to_string()
    } else {
        let mut out = String::new();
        for spec in pathspecs {
            out.push_str(spec);
            out.push('\n');
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Small shared utilities
// ---------------------------------------------------------------------------

fn current_head_oid(repo: &BisectRepo) -> Result<ObjectId> {
    resolve_revision(&repo.git_dir, repo.format, "HEAD")
}

/// The commit currently under test: the detached HEAD in normal mode, or the
/// commit recorded in BISECT_HEAD when bisecting with `--no-checkout`.
fn current_bisect_oid(repo: &BisectRepo, no_checkout: bool) -> Result<ObjectId> {
    if no_checkout {
        let contents = fs::read_to_string(repo.state_path("BISECT_HEAD"))?;
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            return ObjectId::from_hex(repo.format, trimmed);
        }
    }
    current_head_oid(repo)
}

fn commit_subject_of(repo: &BisectRepo, oid: &ObjectId) -> Result<String> {
    let db = repo.db();
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        // Peel tags to their commit if necessary.
        let peeled = sley_rev::peel_to_commit(&db, repo.format, oid)?;
        let object = db.read_object(&peeled)?;
        let commit = Commit::parse(repo.format, &object.body)?;
        return Ok(commit_subject(&commit.message));
    }
    let commit = Commit::parse(repo.format, &object.body)?;
    Ok(commit_subject(&commit.message))
}

fn bisect_option_requires_value(option: &str) -> Result<()> {
    eprintln!(
        "error: option `{}' requires a value",
        option.trim_start_matches('-')
    );
    eprintln!();
    print_bisect_usage();
    Err(GitError::Exit(129))
}

/// Strip a `--name=value` long option, returning the value if `arg` matches.
fn bisect_strip_long_value<'a>(arg: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}=");
    arg.strip_prefix(&prefix)
}

/// Render command-line arguments the way git records them in BISECT_LOG: each
/// argument single-quoted and space-separated, preceded by a leading space.
fn format_log_args(args: &[String]) -> String {
    let mut out = String::new();
    for arg in args {
        out.push(' ');
        out.push_str(&shell_single_quote(arg));
    }
    out
}

/// Render a list of object ids as space-separated full hex, preceded by a
/// leading space (used for the `git bisect good <oid> <oid>` log lines, where
/// git records the resolved oids rather than the user's shorthand).
fn format_args_with_oids(oids: &[ObjectId]) -> String {
    let mut out = String::new();
    for oid in oids {
        out.push(' ');
        out.push_str(&oid.to_hex());
    }
    out
}

fn shell_single_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}
