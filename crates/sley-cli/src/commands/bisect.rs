//! `git bisect` and its subcommands
//! (start/good/bad/skip/reset/log/replay/terms/next/visualize/run).
//!
//! A faithful port of upstream `builtin/bisect.c` + `bisect.c`: the same state
//! files, the same messages, and the same weighted-midpoint selection
//! (`find_bisection` with the `approx_halfway` early exit, `filter_skipped`,
//! and the PRN-driven `skip_away`). The state lives on disk under the git dir:
//!
//! * `BISECT_START` -- the symbolic name (branch) or detached oid HEAD pointed
//!   at when the bisection began; `bisect reset` restores it.
//! * `BISECT_TERMS` -- two lines, `<term-bad>` then `<term-good>`.
//! * `BISECT_NAMES` -- sq-quoted rev-list arguments restricting the search.
//! * `BISECT_LOG` -- a transcript replayed by `bisect replay`.
//! * `BISECT_EXPECTED_REV` / `BISECT_ANCESTORS_OK` / `BISECT_HEAD` /
//!   `BISECT_FIRST_PARENT` / `BISECT_RUN` -- caches and mode markers.
//! * `refs/bisect/<term-bad>` -- the single known-bad commit.
//! * `refs/bisect/<term-good>-<oid>` -- one ref per known-good commit.
//! * `refs/bisect/skip-<oid>` -- one ref per skipped commit.
use crate::*;

// Upstream `enum bisect_error` values; the process exit code is the negation.
const BISECT_OK: i32 = 0;
const BISECT_FAILED: i32 = -1;
const BISECT_ONLY_SKIPPED_LEFT: i32 = -2;
const BISECT_MERGE_BASE_CHECK: i32 = -3;
const BISECT_NO_TESTABLE_COMMIT: i32 = -4;
const BISECT_INTERNAL_SUCCESS_1ST_BAD_FOUND: i32 = -10;
const BISECT_INTERNAL_SUCCESS_MERGE_BASE: i32 = -11;

fn is_bisect_success(res: i32) -> bool {
    res == BISECT_OK
        || res == BISECT_INTERNAL_SUCCESS_1ST_BAD_FOUND
        || res == BISECT_INTERNAL_SUCCESS_MERGE_BASE
}

/// Map an internal bisect code to the command result (exit code = -code).
fn bisect_exit(code: i32) -> Result<()> {
    if is_bisect_success(code) {
        Ok(())
    } else {
        Err(GitError::Exit(-code))
    }
}

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

const VOCAB_BAD: &str = "bad|new";
const VOCAB_GOOD: &str = "good|old";

/// Everything a subcommand needs to manipulate bisection state, resolved once at
/// the top of each command.
struct BisectRepo {
    git_dir: PathBuf,
    /// `None` in a bare repository (which implies `--no-checkout`).
    worktree_root: Option<PathBuf>,
    format: ObjectFormat,
}

impl BisectRepo {
    fn open() -> Result<Self> {
        let cwd = env::current_dir()?;
        let git_dir = discover_git_dir(&cwd)?;
        let worktree_root = sley_worktree::worktree_root_for_git_dir(&git_dir)?;
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

    fn store(&self) -> FileRefStore {
        FileRefStore::new(&self.git_dir, self.format)
    }

    fn state_path(&self, name: &str) -> PathBuf {
        self.git_dir.join(name)
    }

    /// git's `is_empty_or_missing_file(git_path_bisect_start())`, negated.
    fn is_bisecting(&self) -> bool {
        fs::metadata(self.state_path("BISECT_START")).is_ok_and(|meta| meta.len() > 0)
    }

    fn no_checkout(&self) -> bool {
        self.state_path("BISECT_HEAD").exists()
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
        other if other.starts_with('-') => {
            eprintln!("error: unknown option `{}'", other.trim_start_matches('-'));
            eprintln!();
            print_bisect_usage();
            Err(GitError::Exit(129))
        }
        other => {
            // `bad`/`good`/`new`/`old` and user-defined terms dispatch to the
            // state handler; `check_and_set_terms` may initialize BISECT_TERMS
            // or reject a term that mismatches the session's vocabulary.
            let repo = BisectRepo::open()?;
            let mut terms = BisectTerms::default();
            get_terms(&repo, &mut terms);
            if check_and_set_terms(&repo, &mut terms, other).is_err()
                || (other != terms.good && other != terms.bad)
            {
                eprintln!("fatal: unknown command: '{other}'");
                eprintln!();
                print_bisect_usage();
                return Err(GitError::Exit(129));
            }
            let mut out = io::stdout();
            let code = bisect_state(&repo, &mut terms, args, &mut out)?;
            out.flush()?;
            bisect_exit(code)
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
// terms plumbing
// ---------------------------------------------------------------------------

/// Load BISECT_TERMS into `terms` (leaves them untouched when missing),
/// mirroring upstream `get_terms` (returns Err when the file is absent).
fn get_terms(repo: &BisectRepo, terms: &mut BisectTerms) -> bool {
    match fs::read_to_string(repo.state_path("BISECT_TERMS")) {
        Ok(contents) => {
            let mut lines = contents.lines();
            terms.bad = lines.next().unwrap_or("").to_string();
            terms.good = lines.next().unwrap_or("").to_string();
            true
        }
        Err(_) => false,
    }
}

fn check_term_format(term: &str, orig_term: &str) -> Result<i32> {
    // Upstream validates "refs/bisect/<term>" as a refname.
    if term.is_empty()
        || term.contains('/')
        || term.contains(' ')
        || term.contains("..")
        || term.starts_with('-')
        || term.starts_with('.')
        || term.ends_with('.')
        || term.ends_with(".lock")
        || term
            .bytes()
            .any(|byte| byte < 0x20 || byte == 0x7f || b"~^:?*[\\".contains(&byte))
    {
        eprintln!("error: '{term}' is not a valid term");
        return Ok(-1);
    }
    const BUILTINS: &[&str] = &[
        "help",
        "start",
        "skip",
        "next",
        "reset",
        "visualize",
        "view",
        "replay",
        "log",
        "run",
        "terms",
    ];
    if BUILTINS.contains(&term) {
        eprintln!("error: can't use the builtin command '{term}' as a term");
        return Ok(-1);
    }
    if (orig_term != "bad" && (term == "bad" || term == "new"))
        || (orig_term != "good" && (term == "good" || term == "old"))
    {
        eprintln!("error: can't change the meaning of the term '{term}'");
        return Ok(-1);
    }
    Ok(0)
}

fn write_terms(repo: &BisectRepo, bad: &str, good: &str) -> Result<i32> {
    if bad == good {
        eprintln!("error: please use two different terms");
        return Ok(-1);
    }
    if check_term_format(bad, "bad")? != 0 || check_term_format(good, "good")? != 0 {
        return Ok(-1);
    }
    fs::write(repo.state_path("BISECT_TERMS"), format!("{bad}\n{good}\n"))?;
    Ok(0)
}

/// Upstream `check_and_set_terms`: validates a state word against the session's
/// terms, initializing BISECT_TERMS on the first `bad`/`good` or `new`/`old`.
fn check_and_set_terms(repo: &BisectRepo, terms: &mut BisectTerms, cmd: &str) -> Result<()> {
    let has_term_file = fs::metadata(repo.state_path("BISECT_TERMS")).is_ok_and(|m| m.len() > 0);
    if cmd == "skip" || cmd == "start" || cmd == "terms" {
        return Ok(());
    }
    if has_term_file && cmd != terms.bad && cmd != terms.good {
        eprintln!(
            "error: Invalid command: you're currently in a {}/{} bisect",
            terms.bad, terms.good
        );
        return Err(GitError::Exit(1));
    }
    if !has_term_file {
        if cmd == "bad" || cmd == "good" {
            terms.bad = "bad".into();
            terms.good = "good".into();
            if write_terms(repo, "bad", "good")? != 0 {
                return Err(GitError::Exit(1));
            }
        } else if cmd == "new" || cmd == "old" {
            terms.bad = "new".into();
            terms.good = "old".into();
            if write_terms(repo, "new", "old")? != 0 {
                return Err(GitError::Exit(1));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// refs/bisect/* helpers (read via the ref store so packed refs are seen)
// ---------------------------------------------------------------------------

fn bisect_refs(repo: &BisectRepo) -> Result<Vec<(String, ObjectId)>> {
    let store = repo.store();
    let mut out = Vec::new();
    for reference in store.list_refs()? {
        if let Some(rest) = reference.name.strip_prefix("refs/bisect/")
            && let RefTarget::Direct(oid) = reference.target
        {
            out.push((rest.to_string(), oid));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn bisect_bad_oid(repo: &BisectRepo, terms: &BisectTerms) -> Result<Option<ObjectId>> {
    Ok(bisect_refs(repo)?
        .into_iter()
        .find(|(name, _)| *name == terms.bad)
        .map(|(_, oid)| oid))
}

fn bisect_good_oids(repo: &BisectRepo, terms: &BisectTerms) -> Result<Vec<ObjectId>> {
    let prefix = format!("{}-", terms.good);
    Ok(bisect_refs(repo)?
        .into_iter()
        .filter(|(name, _)| name.starts_with(&prefix))
        .map(|(_, oid)| oid)
        .collect())
}

fn bisect_skip_oids(repo: &BisectRepo) -> Result<Vec<ObjectId>> {
    Ok(bisect_refs(repo)?
        .into_iter()
        .filter(|(name, _)| name.starts_with("skip-"))
        .map(|(_, oid)| oid)
        .collect())
}

fn write_loose_bisect_ref(repo: &BisectRepo, name: &str, oid: &ObjectId) -> Result<()> {
    let dir = repo.git_dir.join("refs").join("bisect");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(name), format!("{}\n", oid.to_hex()))?;
    Ok(())
}

/// Upstream `bisect_clean_state`: delete every refs/bisect/* ref (loose or
/// packed), the BISECT_HEAD / BISECT_EXPECTED_REV pseudorefs, and the state
/// files, removing BISECT_START last.
fn bisect_clean_state(repo: &BisectRepo) -> Result<()> {
    let store = repo.store();
    for (name, _) in bisect_refs(repo)? {
        let _ = store.delete_ref(&format!("refs/bisect/{name}"));
        // Loose writes here bypass the store; clear any remaining file too.
        let _ = fs::remove_file(repo.git_dir.join("refs").join("bisect").join(&name));
    }
    for name in ["BISECT_HEAD", "BISECT_EXPECTED_REV"] {
        let _ = fs::remove_file(repo.state_path(name));
    }
    for name in [
        "BISECT_ANCESTORS_OK",
        "BISECT_LOG",
        "BISECT_NAMES",
        "BISECT_RUN",
        "BISECT_TERMS",
        "BISECT_FIRST_PARENT",
        "BISECT_START",
    ] {
        let _ = fs::remove_file(repo.state_path(name));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// log helpers
// ---------------------------------------------------------------------------

fn append_to_bisect_log(repo: &BisectRepo, text: &str) -> Result<()> {
    let mut log = match fs::read_to_string(repo.state_path("BISECT_LOG")) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };
    log.push_str(text);
    fs::write(repo.state_path("BISECT_LOG"), log)?;
    Ok(())
}

fn commit_subject_of(repo: &BisectRepo, oid: &ObjectId) -> Result<String> {
    let db = repo.db();
    let peeled = sley_rev::peel_to_commit(&db, repo.format, oid)?;
    let object = db.read_object(&peeled)?;
    let commit = Commit::parse(repo.format, &object.body)?;
    Ok(commit_subject(&commit.message))
}

/// `# <state>: [<oid>] <subject>` line (upstream `log_commit`).
fn bisect_log_state_line(repo: &BisectRepo, state: &str, oid: &ObjectId) -> Result<String> {
    let subject = commit_subject_of(repo, oid)?;
    Ok(format!("# {state}: [{}] {subject}\n", oid.to_hex()))
}

/// Upstream `bisect_write`: update the state ref and append the log lines.
fn bisect_write(
    repo: &BisectRepo,
    terms: &BisectTerms,
    state: &str,
    rev: &str,
    nolog: bool,
) -> Result<i32> {
    let ref_name = if state == terms.bad {
        terms.bad.clone()
    } else if state == terms.good || state == "skip" {
        format!("{state}-{rev}")
    } else {
        eprintln!("error: Bad bisect_write argument: {state}");
        return Ok(-1);
    };
    let oid = match resolve_revision(&repo.git_dir, repo.format, rev) {
        Ok(oid) => oid,
        Err(_) => {
            eprintln!("error: couldn't get the oid of the rev '{rev}'");
            return Ok(-1);
        }
    };
    write_loose_bisect_ref(repo, &ref_name, &oid)?;
    append_to_bisect_log(repo, &bisect_log_state_line(repo, state, &oid)?)?;
    if !nolog {
        append_to_bisect_log(repo, &format!("git bisect {state} {rev}\n"))?;
    }
    Ok(0)
}

/// Render args the way git records them: each sq-quoted, space-prefixed.
fn sq_quote_args<I: IntoIterator<Item = S>, S: AsRef<str>>(args: I) -> String {
    let mut out = String::new();
    for arg in args {
        out.push(' ');
        out.push('\'');
        for ch in arg.as_ref().chars() {
            if ch == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(ch);
            }
        }
        out.push('\'');
    }
    out
}

// ---------------------------------------------------------------------------
// status / next-check
// ---------------------------------------------------------------------------

struct BisectState {
    nr_good: usize,
    nr_bad: usize,
}

fn bisect_status(repo: &BisectRepo, terms: &BisectTerms) -> Result<BisectState> {
    Ok(BisectState {
        nr_good: bisect_good_oids(repo, terms)?.len(),
        nr_bad: usize::from(bisect_bad_oid(repo, terms)?.is_some()),
    })
}

/// Print to stdout AND append `# <text>` to BISECT_LOG (upstream
/// `bisect_log_printf`).
fn bisect_log_printf(repo: &BisectRepo, out: &mut dyn Write, text: &str) -> Result<()> {
    write!(out, "{text}")?;
    append_to_bisect_log(repo, &format!("# {text}"))?;
    Ok(())
}

fn bisect_print_status(repo: &BisectRepo, terms: &BisectTerms, out: &mut dyn Write) -> Result<()> {
    let state = bisect_status(repo, terms)?;
    if state.nr_good > 0 && state.nr_bad > 0 {
        return Ok(());
    }
    if state.nr_good == 0 && state.nr_bad == 0 {
        bisect_log_printf(repo, out, "status: waiting for both good and bad commits\n")?;
    } else if state.nr_good > 0 {
        let plural = if state.nr_good == 1 {
            "commit"
        } else {
            "commits"
        };
        bisect_log_printf(
            repo,
            out,
            &format!(
                "status: waiting for bad commit, {} good {plural} known\n",
                state.nr_good
            ),
        )?;
    } else {
        bisect_log_printf(
            repo,
            out,
            "status: waiting for good commit(s), bad commit known\n",
        )?;
    }
    Ok(())
}

/// Upstream `decide_next`: 0 = proceed, -1 = cannot.
fn decide_next(
    repo: &BisectRepo,
    terms: &BisectTerms,
    current_term: Option<&str>,
    missing_good: bool,
    missing_bad: bool,
) -> i32 {
    if !missing_good && !missing_bad {
        return 0;
    }
    let Some(current_term) = current_term else {
        return -1;
    };
    if missing_good && !missing_bad && current_term == terms.good {
        // Have bad (or new) but not good (or old): warn, and proceed when
        // stdin is not a terminal (the test environment).
        eprintln!("warning: bisecting only with a {} commit", terms.bad);
        return 0;
    }
    if repo.is_bisecting() {
        eprintln!(
            "error: You need to give me at least one {VOCAB_BAD} and {VOCAB_GOOD} revision.\nYou can use \"git bisect {VOCAB_BAD}\" and \"git bisect {VOCAB_GOOD}\" for that."
        );
    } else {
        eprintln!(
            "error: You need to start by \"git bisect start\".\nYou then need to give me at least one {VOCAB_GOOD} and {VOCAB_BAD} revision.\nYou can use \"git bisect {VOCAB_GOOD}\" and \"git bisect {VOCAB_BAD}\" for that."
        );
    }
    -1
}

fn bisect_next_check(repo: &BisectRepo, terms: &BisectTerms, current_term: Option<&str>) -> i32 {
    let state = match bisect_status(repo, terms) {
        Ok(state) => state,
        Err(_) => return -1,
    };
    decide_next(
        repo,
        terms,
        current_term,
        state.nr_good == 0,
        state.nr_bad == 0,
    )
}

fn bisect_autostart(repo: &BisectRepo, terms: &mut BisectTerms) -> i32 {
    if repo.is_bisecting() {
        return 0;
    }
    eprintln!("You need to start by \"git bisect start\"\n");
    // Non-interactive stdin: do not autostart, fail like upstream.
    let _ = terms;
    -1
}

// ---------------------------------------------------------------------------
// state (good/bad/new/old/<term>) and skip
// ---------------------------------------------------------------------------

/// The commit currently under test: BISECT_HEAD when present, else HEAD.
fn current_bisect_oid(repo: &BisectRepo) -> Result<ObjectId> {
    if let Ok(contents) = fs::read_to_string(repo.state_path("BISECT_HEAD")) {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            return ObjectId::from_hex(repo.format, trimmed);
        }
    }
    resolve_revision(&repo.git_dir, repo.format, "HEAD")
}

/// Upstream `bisect_state`: argv[0] is the state word, the rest are revs.
fn bisect_state(
    repo: &BisectRepo,
    terms: &mut BisectTerms,
    argv: &[String],
    out: &mut dyn Write,
) -> Result<i32> {
    if argv.is_empty() {
        eprintln!("error: Please call `--bisect-state` with at least one argument");
        return Ok(BISECT_FAILED);
    }
    if bisect_autostart(repo, terms) != 0 {
        return Ok(BISECT_FAILED);
    }
    let state = argv[0].clone();
    if check_and_set_terms(repo, terms, &state).is_err()
        || !(state == terms.good || state == terms.bad || state == "skip")
    {
        return Ok(BISECT_FAILED);
    }
    let rev_args = &argv[1..];
    if rev_args.len() > 1 && state == terms.bad {
        eprintln!(
            "error: 'git bisect {}' can take only one argument.",
            terms.bad
        );
        return Ok(BISECT_FAILED);
    }

    let db = repo.db();
    let mut revs: Vec<ObjectId> = Vec::new();
    if rev_args.is_empty() {
        let oid = match current_bisect_oid(repo) {
            Ok(oid) => oid,
            Err(_) => {
                eprintln!("error: Bad rev input: HEAD");
                return Ok(BISECT_FAILED);
            }
        };
        revs.push(oid);
    }
    // All input revs are checked before any write so junk revs leave no state.
    for arg in rev_args {
        let oid = match resolve_revision(&repo.git_dir, repo.format, arg) {
            Ok(oid) => oid,
            Err(_) => {
                eprintln!("error: Bad rev input: {arg}");
                return Ok(BISECT_FAILED);
            }
        };
        let commit = match sley_rev::peel_to_commit(&db, repo.format, &oid) {
            Ok(commit) => commit,
            Err(_) => {
                eprintln!("fatal: Bad rev input (not a commit): {arg}");
                return Err(GitError::Exit(128));
            }
        };
        revs.push(commit);
    }

    let mut verify_expected = true;
    let expected: Option<ObjectId> = fs::read_to_string(repo.state_path("BISECT_EXPECTED_REV"))
        .ok()
        .and_then(|contents| ObjectId::from_hex(repo.format, contents.trim()).ok());
    if expected.is_none() {
        verify_expected = false;
    }
    for oid in &revs {
        if bisect_write(repo, terms, &state, &oid.to_hex(), false)? != 0 {
            return Ok(BISECT_FAILED);
        }
        if verify_expected && Some(*oid) != expected {
            let _ = fs::remove_file(repo.state_path("BISECT_ANCESTORS_OK"));
            let _ = fs::remove_file(repo.state_path("BISECT_EXPECTED_REV"));
            verify_expected = false;
        }
    }
    bisect_auto_next(repo, terms, out)
}

fn cmd_bisect_skip(args: &[String]) -> Result<()> {
    let repo = BisectRepo::open()?;
    let mut terms = BisectTerms::default();
    get_terms(&repo, &mut terms);

    let mut argv_state: Vec<String> = vec!["skip".to_string()];
    for arg in args {
        if arg.contains("..") {
            // A range skips every commit the range expression selects.
            let db = repo.db();
            let (left, right) = arg.split_once("..").unwrap_or(("", ""));
            let left = if left.is_empty() { "HEAD" } else { left };
            let right = if right.is_empty() { "HEAD" } else { right };
            let left_oid = resolve_revision(&repo.git_dir, repo.format, left)
                .and_then(|oid| sley_rev::peel_to_commit(&db, repo.format, &oid));
            let right_oid = resolve_revision(&repo.git_dir, repo.format, right)
                .and_then(|oid| sley_rev::peel_to_commit(&db, repo.format, &oid));
            let (Ok(left_oid), Ok(right_oid)) = (left_oid, right_oid) else {
                eprintln!("fatal: Bad rev input: {arg}");
                return Err(GitError::Exit(128));
            };
            let mut excluded: HashSet<ObjectId> = HashSet::new();
            for record in sley_rev::walk_commits(&db, repo.format, [left_oid])? {
                excluded.insert(record.oid);
            }
            for record in sley_rev::walk_commits(&db, repo.format, [right_oid])? {
                if !excluded.contains(&record.oid) {
                    argv_state.push(record.oid.to_hex());
                }
            }
        } else {
            argv_state.push(arg.clone());
        }
    }
    let mut out = io::stdout();
    let code = bisect_state(&repo, &mut terms, &argv_state, &mut out)?;
    out.flush()?;
    bisect_exit(code)
}

// ---------------------------------------------------------------------------
// next / auto-next
// ---------------------------------------------------------------------------

fn cmd_bisect_next(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        eprintln!("error: 'git bisect next' requires 0 arguments");
        return Err(GitError::Exit(1));
    }
    let repo = BisectRepo::open()?;
    let mut terms = BisectTerms::default();
    get_terms(&repo, &mut terms);
    let mut out = io::stdout();
    let code = bisect_next(&repo, &mut terms, &mut out)?;
    out.flush()?;
    bisect_exit(code)
}

fn bisect_next(repo: &BisectRepo, terms: &mut BisectTerms, out: &mut dyn Write) -> Result<i32> {
    if bisect_autostart(repo, terms) != 0 {
        return Ok(BISECT_FAILED);
    }
    let good_term = terms.good.clone();
    if bisect_next_check(repo, terms, Some(&good_term)) != 0 {
        return Ok(BISECT_FAILED);
    }
    let res = bisect_next_all(repo, terms, out)?;
    if res == BISECT_INTERNAL_SUCCESS_1ST_BAD_FOUND {
        // Record the conclusion in BISECT_LOG.
        if let Some(bad) = bisect_bad_oid(repo, terms)? {
            let subject = commit_subject_of(repo, &bad)?;
            append_to_bisect_log(
                repo,
                &format!(
                    "# first {} commit: [{}] {subject}\n",
                    terms.bad,
                    bad.to_hex()
                ),
            )?;
        }
        return Ok(BISECT_INTERNAL_SUCCESS_1ST_BAD_FOUND);
    }
    if res == BISECT_ONLY_SKIPPED_LEFT {
        bisect_log_skipped_commits(repo, terms)?;
        return Ok(BISECT_ONLY_SKIPPED_LEFT);
    }
    Ok(res)
}

fn bisect_auto_next(
    repo: &BisectRepo,
    terms: &mut BisectTerms,
    out: &mut dyn Write,
) -> Result<i32> {
    if bisect_next_check(repo, terms, None) != 0 {
        bisect_print_status(repo, terms, out)?;
        return Ok(BISECT_OK);
    }
    bisect_next(repo, terms, out)
}

/// Append the `# only skipped commits left to test` block to BISECT_LOG
/// (upstream `bisect_skipped_commits`), listing `bad ^goods` newest-first.
fn bisect_log_skipped_commits(repo: &BisectRepo, terms: &BisectTerms) -> Result<()> {
    let Some(bad) = bisect_bad_oid(repo, terms)? else {
        return Ok(());
    };
    let goods = bisect_good_oids(repo, terms)?;
    let candidates = bisect_candidate_records(repo, &bad, &goods, repo.first_parent_mode())?;
    let mut text = String::from("# only skipped commits left to test\n");
    for record in &candidates {
        text.push_str(&format!(
            "# possible first {} commit: [{}] {}\n",
            terms.bad,
            record.oid.to_hex(),
            commit_subject(&record.commit.message)
        ));
    }
    append_to_bisect_log(repo, &text)?;
    Ok(())
}

impl BisectRepo {
    fn first_parent_mode(&self) -> bool {
        self.state_path("BISECT_FIRST_PARENT").exists()
    }
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

fn cmd_bisect_start(args: &[String]) -> Result<()> {
    let repo = BisectRepo::open()?;
    let mut terms = BisectTerms::default();
    let mut out = io::stdout();
    let code = bisect_start(&repo, &mut terms, args, &mut out)?;
    out.flush()?;
    bisect_exit(code)
}

fn bisect_start(
    repo: &BisectRepo,
    terms: &mut BisectTerms,
    args: &[String],
    out: &mut dyn Write,
) -> Result<i32> {
    let mut no_checkout = repo.worktree_root.is_none();
    let mut first_parent_only = false;
    let mut must_write_terms = false;
    let mut revs: Vec<ObjectId> = Vec::new();

    let has_double_dash = args.iter().any(|arg| arg == "--");
    let db = repo.db();

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            break;
        } else if arg == "--no-checkout" {
            no_checkout = true;
        } else if arg == "--first-parent" {
            first_parent_only = true;
        } else if arg == "--term-good" || arg == "--term-old" {
            index += 1;
            let Some(value) = args.get(index) else {
                eprintln!("error: '' is not a valid term");
                return Ok(BISECT_FAILED);
            };
            must_write_terms = true;
            terms.good = value.clone();
        } else if let Some(value) = arg
            .strip_prefix("--term-good=")
            .or_else(|| arg.strip_prefix("--term-old="))
        {
            must_write_terms = true;
            terms.good = value.to_string();
        } else if arg == "--term-bad" || arg == "--term-new" {
            index += 1;
            let Some(value) = args.get(index) else {
                eprintln!("error: '' is not a valid term");
                return Ok(BISECT_FAILED);
            };
            must_write_terms = true;
            terms.bad = value.clone();
        } else if let Some(value) = arg
            .strip_prefix("--term-bad=")
            .or_else(|| arg.strip_prefix("--term-new="))
        {
            must_write_terms = true;
            terms.bad = value.to_string();
        } else if arg.starts_with("--") {
            eprintln!("error: unrecognized option: '{arg}'");
            return Ok(BISECT_FAILED);
        } else if let Ok(oid) = resolve_revision(&repo.git_dir, repo.format, arg)
            .and_then(|oid| sley_rev::peel_to_commit(&db, repo.format, &oid))
        {
            revs.push(oid);
        } else if has_double_dash {
            eprintln!("fatal: '{arg}' does not appear to be a valid revision");
            return Err(GitError::Exit(128));
        } else {
            break;
        }
        index += 1;
    }
    let pathspec_pos = index;

    if !revs.is_empty() {
        must_write_terms = true;
    }
    // First rev is bad, the rest are good.
    let states: Vec<String> = revs
        .iter()
        .enumerate()
        .map(|(idx, _)| {
            if idx == 0 {
                terms.bad.clone()
            } else {
                terms.good.clone()
            }
        })
        .collect();

    // Verify HEAD and figure out where the bisection starts from.
    let store = repo.store();
    let head_target = store.read_ref("HEAD")?;
    if head_target.is_none() {
        eprintln!("error: bad HEAD - I need a HEAD");
        return Ok(BISECT_FAILED);
    }

    let start_head: String = if repo.is_bisecting() {
        // Already bisecting: move back to where the previous session started.
        let recorded = fs::read_to_string(repo.state_path("BISECT_START"))?;
        let recorded = recorded.trim().to_string();
        if !no_checkout
            && cmd_checkout(&[
                "--ignore-other-worktrees".to_string(),
                recorded.clone(),
                "--".to_string(),
            ])
            .is_err()
        {
            eprintln!(
                "error: checking out '{recorded}' failed. Try 'git bisect start <valid-branch>'."
            );
            return Ok(BISECT_FAILED);
        }
        recorded
    } else {
        match head_target {
            Some(RefTarget::Symbolic(name)) => match name.strip_prefix("refs/heads/") {
                Some(branch) => branch.to_string(),
                None => {
                    eprintln!("error: bad HEAD - strange symbolic ref");
                    return Ok(BISECT_FAILED);
                }
            },
            Some(RefTarget::Direct(oid)) => oid.to_hex(),
            None => unreachable!("checked above"),
        }
    };

    // Get rid of any old bisect state.
    bisect_clean_state(repo)?;

    let res = (|| -> Result<i32> {
        fs::write(repo.state_path("BISECT_START"), format!("{start_head}\n"))?;
        if first_parent_only {
            fs::write(repo.state_path("BISECT_FIRST_PARENT"), "\n")?;
        }
        if no_checkout {
            let oid = match resolve_revision(&repo.git_dir, repo.format, &start_head) {
                Ok(oid) => oid,
                Err(_) => {
                    eprintln!("error: invalid ref: '{start_head}'");
                    return Ok(BISECT_FAILED);
                }
            };
            fs::write(
                repo.state_path("BISECT_HEAD"),
                format!("{}\n", oid.to_hex()),
            )?;
        }

        // Record the rev-list restriction (the args from the first non-rev on),
        // replicating upstream's `pathspec_pos < argc - 1` quirk.
        let names = if pathspec_pos + 1 < args.len() {
            format!("{}\n", sq_quote_args(args[pathspec_pos..].iter()))
        } else {
            "\n".to_string()
        };
        fs::write(repo.state_path("BISECT_NAMES"), names)?;

        for (state, oid) in states.iter().zip(&revs) {
            if bisect_write(repo, terms, state, &oid.to_hex(), true)? != 0 {
                return Ok(BISECT_FAILED);
            }
        }

        if must_write_terms && write_terms(repo, &terms.bad, &terms.good)? != 0 {
            return Ok(BISECT_FAILED);
        }

        append_to_bisect_log(
            repo,
            &format!("git bisect start{}\n", sq_quote_args(args.iter())),
        )?;
        Ok(BISECT_OK)
    })()?;
    if res != BISECT_OK {
        return Ok(res);
    }

    let res = bisect_auto_next(repo, terms, out)?;
    if !is_bisect_success(res) {
        bisect_clean_state(repo)?;
    }
    Ok(res)
}

// ---------------------------------------------------------------------------
// reset / log / replay / terms / visualize
// ---------------------------------------------------------------------------

fn cmd_bisect_reset(args: &[String]) -> Result<()> {
    if args.len() > 1 {
        eprintln!("error: 'git bisect reset' requires either no argument or a commit");
        return Err(GitError::Exit(1));
    }
    let repo = BisectRepo::open()?;
    bisect_exit(bisect_reset(&repo, args.first().map(String::as_str))?)
}

fn bisect_reset(repo: &BisectRepo, commit: Option<&str>) -> Result<i32> {
    let branch: String = match commit {
        None => {
            // Upstream prints "We are not bisecting." only when BISECT_START
            // exists but is empty (strbuf_read_file(...) == 0); a missing file
            // resets silently.
            match fs::read_to_string(repo.state_path("BISECT_START")) {
                Ok(contents) if contents.is_empty() => {
                    println!("We are not bisecting.");
                    String::new()
                }
                Ok(contents) => contents.trim_end().to_string(),
                Err(_) => String::new(),
            }
        }
        Some(commit) => {
            let db = repo.db();
            if resolve_revision(&repo.git_dir, repo.format, commit)
                .and_then(|oid| sley_rev::peel_to_commit(&db, repo.format, &oid))
                .is_err()
            {
                eprintln!("error: '{commit}' is not a valid commit");
                return Ok(BISECT_FAILED);
            }
            commit.to_string()
        }
    };

    if !branch.is_empty()
        && !repo.state_path("BISECT_HEAD").exists()
        && cmd_checkout(&[
            "--ignore-other-worktrees".to_string(),
            branch.clone(),
            "--".to_string(),
        ])
        .is_err()
    {
        eprintln!(
            "error: could not check out original HEAD '{branch}'. Try 'git bisect reset <commit>'."
        );
        return Ok(BISECT_FAILED);
    }
    bisect_clean_state(repo)?;
    Ok(BISECT_OK)
}

fn cmd_bisect_log(args: &[String]) -> Result<()> {
    let _ = args; // upstream ignores extra arguments
    let repo = BisectRepo::open()?;
    let log_path = repo.state_path("BISECT_LOG");
    let empty_or_missing = fs::metadata(&log_path)
        .map(|m| m.len() == 0)
        .unwrap_or(true);
    if empty_or_missing {
        eprintln!("error: We are not bisecting.");
        return Err(GitError::Exit(1));
    }
    let log = fs::read(&log_path)?;
    let mut stdout = io::stdout();
    stdout.write_all(&log)?;
    stdout.flush()?;
    Ok(())
}

fn cmd_bisect_replay(args: &[String]) -> Result<()> {
    if args.len() != 1 {
        eprintln!("error: no logfile given");
        return Err(GitError::Exit(1));
    }
    let filename = &args[0];
    let repo = BisectRepo::open()?;
    let mut terms = BisectTerms::default();
    bisect_exit(bisect_replay(&repo, &mut terms, filename)?)
}

fn bisect_replay(repo: &BisectRepo, terms: &mut BisectTerms, filename: &str) -> Result<i32> {
    let empty_or_missing = fs::metadata(filename).map(|m| m.len() == 0).unwrap_or(true);
    if empty_or_missing {
        eprintln!("error: cannot read file '{filename}' for replaying");
        return Ok(BISECT_FAILED);
    }
    if bisect_reset(repo, None)? != 0 {
        return Ok(BISECT_FAILED);
    }
    let contents = fs::read_to_string(filename)?;
    let mut out = io::stdout();
    let mut res = BISECT_OK;
    for line in contents.lines() {
        if res != BISECT_OK {
            break;
        }
        res = process_replay_line(repo, terms, line.trim_end_matches('\r'), &mut out)?;
    }
    out.flush()?;
    if res != BISECT_OK {
        return Ok(BISECT_FAILED);
    }
    bisect_auto_next(repo, terms, &mut io::stdout())
}

fn process_replay_line(
    repo: &BisectRepo,
    terms: &mut BisectTerms,
    line: &str,
    out: &mut dyn Write,
) -> Result<i32> {
    let p = line.trim_start();
    let rest = if let Some(rest) = p.strip_prefix("git bisect") {
        rest
    } else if let Some(rest) = p.strip_prefix("git-bisect") {
        rest
    } else {
        return Ok(BISECT_OK);
    };
    if !rest.starts_with([' ', '\t']) {
        return Ok(BISECT_OK);
    }
    let rest = rest.trim_start();
    let (word, rev) = match rest.find([' ', '\t']) {
        Some(pos) => (&rest[..pos], rest[pos..].trim_start()),
        None => (rest, ""),
    };

    get_terms(repo, terms);
    if check_and_set_terms(repo, terms, word).is_err() {
        return Ok(BISECT_FAILED);
    }

    if word == "start" {
        let argv = sq_dequote_args(rev);
        return bisect_start(repo, terms, &argv, out);
    }
    if word == terms.good || word == terms.bad || word == "skip" {
        return bisect_write(repo, terms, word, rev, false);
    }
    if word == "terms" {
        let argv = sq_dequote_args(rev);
        return bisect_terms_print(repo, terms, argv.first().map(String::as_str));
    }
    eprintln!("error: '{word}'?? what are you talking about?");
    Ok(BISECT_FAILED)
}

/// Split an sq-quoted argument string back into tokens (upstream
/// `sq_dequote_to_strvec`, lenient about unquoted words).
fn sq_dequote_args(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut chars = input.chars().peekable();
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
                loop {
                    match chars.next() {
                        Some('\'') => {
                            // `'\''` escape: backslash-quote-quote
                            if chars.peek() == Some(&'\\') {
                                let mut lookahead = chars.clone();
                                lookahead.next();
                                if lookahead.peek() == Some(&'\'') {
                                    chars.next();
                                    chars.next();
                                    current.push('\'');
                                    continue;
                                }
                            }
                            break;
                        }
                        Some(other) => current.push(other),
                        None => break,
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
    tokens
}

fn cmd_bisect_terms(args: &[String]) -> Result<()> {
    if args.len() > 1 {
        eprintln!("error: 'git bisect terms' requires 0 or 1 argument");
        return Err(GitError::Exit(1));
    }
    let repo = BisectRepo::open()?;
    let mut terms = BisectTerms::default();
    bisect_exit(bisect_terms_print(
        &repo,
        &mut terms,
        args.first().map(String::as_str),
    )?)
}

fn bisect_terms_print(
    repo: &BisectRepo,
    terms: &mut BisectTerms,
    option: Option<&str>,
) -> Result<i32> {
    if !get_terms(repo, terms) {
        eprintln!("error: no terms defined");
        return Ok(BISECT_FAILED);
    }
    match option {
        None => {
            println!("Your current terms are {} for the old state", terms.good);
            println!("and {} for the new state.", terms.bad);
            Ok(BISECT_OK)
        }
        Some("--term-good") | Some("--term-old") => {
            println!("{}", terms.good);
            Ok(BISECT_OK)
        }
        Some("--term-bad") | Some("--term-new") => {
            println!("{}", terms.bad);
            Ok(BISECT_OK)
        }
        Some(other) => {
            eprintln!(
                "error: invalid argument {other} for 'git bisect terms'.\nSupported options are: --term-good|--term-old and --term-bad|--term-new."
            );
            Ok(BISECT_FAILED)
        }
    }
}

fn cmd_bisect_visualize(_args: &[String]) -> Result<()> {
    let repo = BisectRepo::open()?;
    let mut terms = BisectTerms::default();
    get_terms(&repo, &mut terms);
    if bisect_next_check(&repo, &terms, None) != 0 {
        return Err(GitError::Exit(1));
    }
    Err(GitError::Unsupported(
        "git bisect visualize is not implemented".into(),
    ))
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

fn cmd_bisect_run(args: &[String]) -> Result<()> {
    if args.is_empty() {
        eprintln!("error: 'git bisect run' failed: no command provided.");
        return Err(GitError::Exit(1));
    }
    let repo = BisectRepo::open()?;
    let mut terms = BisectTerms::default();
    get_terms(&repo, &mut terms);
    bisect_exit(bisect_run(&repo, &mut terms, args)?)
}

/// Run `command` through the shell, returning the child's exit code (negative
/// when killed by a signal, mirroring run_command).
fn do_bisect_run(command: &str) -> Result<i32> {
    println!("running {command}");
    io::stdout().flush()?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .status()?;
    match status.code() {
        Some(code) => Ok(code),
        None => Ok(-1),
    }
}

fn verify_good(repo: &BisectRepo, terms: &BisectTerms, command: &str) -> Result<i32> {
    let goods = bisect_good_oids(repo, terms)?;
    let Some(good_rev) = goods.first().copied() else {
        return Ok(-1);
    };
    let no_checkout = repo.no_checkout();
    let current = match current_bisect_oid(repo) {
        Ok(oid) => oid,
        Err(_) => return Ok(-1),
    };
    let mut sink = Vec::new();
    if bisect_checkout(repo, &good_rev, no_checkout, &mut sink)? != BISECT_OK {
        return Ok(-1);
    }
    let rc = do_bisect_run(command)?;
    let mut sink = Vec::new();
    if bisect_checkout(repo, &current, no_checkout, &mut sink)? != BISECT_OK {
        return Ok(-1);
    }
    Ok(rc)
}

fn bisect_run(repo: &BisectRepo, terms: &mut BisectTerms, args: &[String]) -> Result<i32> {
    if bisect_next_check(repo, terms, None) != 0 {
        return Ok(BISECT_FAILED);
    }
    if args.is_empty() {
        eprintln!("error: bisect run failed: no command provided.");
        return Ok(BISECT_FAILED);
    }
    let command = sq_quote_args(args.iter()).trim_start().to_string();
    let mut is_first_run = true;
    loop {
        let res = do_bisect_run(&command)?;

        // Exit code 126 and 127 can come from the shell when the script is
        // missing or not executable; verify with a known-good revision.
        if is_first_run && (res == 126 || res == 127) {
            let rc = verify_good(repo, terms, &command)?;
            is_first_run = false;
            if !(0..128).contains(&rc) {
                eprintln!("error: unable to verify {command} on good revision");
                return Ok(BISECT_FAILED);
            }
            if rc == res {
                eprintln!("error: bogus exit code {rc} for good revision");
                return Ok(BISECT_FAILED);
            }
        }

        if !(0..128).contains(&res) {
            eprintln!("error: bisect run failed: exit code {res} from {command} is < 0 or >= 128");
            return Ok(res);
        }

        let new_state = if res == 125 {
            "skip".to_string()
        } else if res == 0 {
            terms.good.clone()
        } else {
            terms.bad.clone()
        };

        // Upstream redirects the state step's stdout into BISECT_RUN, then
        // prints the file.
        let mut buffer: Vec<u8> = Vec::new();
        let state_res = bisect_state(repo, terms, std::slice::from_ref(&new_state), &mut buffer)?;
        fs::write(repo.state_path("BISECT_RUN"), &buffer)?;
        io::stdout().write_all(&buffer)?;
        io::stdout().flush()?;

        if state_res == BISECT_ONLY_SKIPPED_LEFT {
            eprintln!("error: bisect run cannot continue any more");
            return Ok(state_res);
        } else if state_res == BISECT_INTERNAL_SUCCESS_MERGE_BASE {
            println!("bisect run success");
            return Ok(BISECT_OK);
        } else if state_res == BISECT_INTERNAL_SUCCESS_1ST_BAD_FOUND {
            println!("bisect found first bad commit");
            return Ok(BISECT_OK);
        } else if state_res != BISECT_OK {
            eprintln!(
                "error: bisect run failed: 'git bisect {new_state}' exited with error code {state_res}"
            );
            return Ok(state_res);
        }
    }
}

// ---------------------------------------------------------------------------
// The bisection core: candidates, weights, midpoint (upstream bisect.c)
// ---------------------------------------------------------------------------

/// `bad ^goods` in rev-list order (newest-first date order), first-parent
/// limited on the bad side when requested.
fn bisect_candidate_records(
    repo: &BisectRepo,
    bad: &ObjectId,
    goods: &[ObjectId],
    first_parent: bool,
) -> Result<Vec<sley_rev::CommitRecord>> {
    let db = repo.db();
    let mut excluded: HashSet<ObjectId> = HashSet::new();
    for good in goods {
        for record in sley_rev::walk_commits(&db, repo.format, [*good])? {
            excluded.insert(record.oid);
        }
    }
    let records = rev_list_walk_commits(&db, repo.format, [*bad], first_parent)?;
    let kept: Vec<sley_rev::CommitRecord> = records
        .into_iter()
        .filter(|record| !excluded.contains(&record.oid))
        .collect();
    let refs: Vec<&sley_rev::CommitRecord> = kept.iter().collect();
    let ordered = rev_list_date_order(refs)?;
    Ok(ordered.into_iter().cloned().collect())
}

/// Read the BISECT_NAMES pathspec restriction (sq-quoted; `--` tokens dropped).
fn bisect_pathspec(repo: &BisectRepo) -> Result<Option<sley_rev::Pathspec>> {
    let contents = match fs::read_to_string(repo.state_path("BISECT_NAMES")) {
        Ok(contents) => contents,
        Err(_) => return Ok(None),
    };
    let mut specs: Vec<String> = Vec::new();
    for line in contents.lines() {
        for token in sq_dequote_args(line.trim()) {
            if token != "--" && !token.is_empty() {
                specs.push(token);
            }
        }
    }
    if specs.is_empty() {
        return Ok(None);
    }
    let pathspec = sley_rev::Pathspec::parse(
        specs.iter().map(|spec| spec.as_bytes()),
        sley_rev::PathspecMatchMagic::default(),
    )
    .map_err(|err| GitError::Command(format!("bad pathspec: {err:?}")))?;
    Ok(Some(pathspec))
}

/// The weighted-midpoint bisection core (`do_find_bisection`, `count_distance`,
/// the `approx_halfway` early exit, and the `filter_skipped` + `skip_away` skip
/// machinery) lives in [`sley_rev::bisect`] — a shared primitive used both here
/// and by `rev-list --bisect`.
use sley_rev::bisect::{SkipFilter, do_find_bisection, estimate_bisect_steps, managed_skipped};

fn error_if_skipped_commits(
    repo: &BisectRepo,
    terms: &BisectTerms,
    tried: &[ObjectId],
    bad: Option<&ObjectId>,
    out: &mut dyn Write,
) -> Result<i32> {
    if tried.is_empty() {
        return Ok(BISECT_OK);
    }
    writeln!(
        out,
        "There are only 'skip'ped commits left to test.\nThe first {} commit could be any of:",
        terms.bad
    )?;
    for oid in tried {
        writeln!(out, "{}", oid.to_hex())?;
    }
    if let Some(bad) = bad {
        writeln!(out, "{}", bad.to_hex())?;
    }
    writeln!(out, "We cannot bisect more!")?;
    let _ = repo;
    Ok(BISECT_ONLY_SKIPPED_LEFT)
}

/// Check out (or record, with --no-checkout) the next rev and print
/// `[<oid>] <subject>` (upstream `bisect_checkout`).
fn bisect_checkout(
    repo: &BisectRepo,
    rev: &ObjectId,
    no_checkout: bool,
    out: &mut dyn Write,
) -> Result<i32> {
    fs::write(
        repo.state_path("BISECT_EXPECTED_REV"),
        format!("{}\n", rev.to_hex()),
    )?;
    if no_checkout {
        fs::write(
            repo.state_path("BISECT_HEAD"),
            format!("{}\n", rev.to_hex()),
        )?;
    } else {
        let Some(worktree_root) = &repo.worktree_root else {
            return Ok(BISECT_FAILED);
        };
        let committer = commit_identity_from_env("COMMITTER")?;
        let old = resolve_revision(&repo.git_dir, repo.format, "HEAD")
            .map(|oid| oid.to_hex())
            .unwrap_or_else(|_| "HEAD".to_string());
        let message = format!("checkout: moving from {old} to {}", rev.to_hex());
        if let Err(err) = sley_worktree::checkout_detached(
            worktree_root,
            &repo.git_dir,
            repo.format,
            rev,
            committer,
            message.into_bytes(),
        ) {
            // A missing tree/object dies the way `git checkout` does.
            if let GitError::NotFound(kind) = &err {
                let text = kind.to_string();
                if let Some(hex) = text
                    .split(|ch: char| !ch.is_ascii_hexdigit())
                    .find(|token| token.len() >= 40)
                {
                    eprintln!("fatal: unable to read tree ({hex})");
                }
            }
            return Ok(BISECT_FAILED);
        }
    }
    let subject = commit_subject_of(repo, rev)?;
    writeln!(out, "[{}] {subject}", rev.to_hex())?;
    Ok(BISECT_OK)
}

/// Independent merge bases of `bad` against all `goods` (upstream
/// `repo_get_merge_bases_many`, approximated by the deduped union of pairwise
/// bases with dominated entries removed).
fn bisect_merge_bases(
    repo: &BisectRepo,
    bad: &ObjectId,
    goods: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let db = repo.db();
    let mut bases: Vec<ObjectId> = Vec::new();
    for good in goods {
        for base in merge_bases(&repo.git_dir, &db, repo.format, bad, good)? {
            if !bases.contains(&base) {
                bases.push(base);
            }
        }
    }
    if bases.len() > 1 {
        // Drop bases that are ancestors of another base.
        let mut independent = Vec::new();
        for (idx, base) in bases.iter().enumerate() {
            let mut dominated = false;
            for (other_idx, other) in bases.iter().enumerate() {
                if idx != other_idx
                    && sley_rev::is_ancestor(&repo.git_dir, repo.format, &db, base, other)?
                {
                    dominated = true;
                    break;
                }
            }
            if !dominated {
                independent.push(*base);
            }
        }
        bases = independent;
    }
    Ok(bases)
}

fn is_expected_rev(repo: &BisectRepo, oid: &ObjectId) -> bool {
    fs::read_to_string(repo.state_path("BISECT_EXPECTED_REV"))
        .ok()
        .and_then(|contents| ObjectId::from_hex(repo.format, contents.trim()).ok())
        .is_some_and(|expected| expected == *oid)
}

fn join_oids_hex(oids: &[ObjectId]) -> String {
    oids.iter()
        .map(ObjectId::to_hex)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Upstream `check_merge_bases`.
fn check_merge_bases(
    repo: &BisectRepo,
    terms: &BisectTerms,
    bad: &ObjectId,
    goods: &[ObjectId],
    skipped: &HashSet<ObjectId>,
    no_checkout: bool,
    out: &mut dyn Write,
) -> Result<i32> {
    let bases = bisect_merge_bases(repo, bad, goods)?;
    for base in &bases {
        if base == bad {
            // handle_bad_merge_base
            if is_expected_rev(repo, bad) {
                let bad_hex = bad.to_hex();
                let good_hex = join_oids_hex(goods);
                if terms.bad == "bad" && terms.good == "good" {
                    eprintln!(
                        "The merge base {bad_hex} is bad.\nThis means the bug has been fixed between {bad_hex} and [{good_hex}]."
                    );
                } else if terms.bad == "new" && terms.good == "old" {
                    eprintln!(
                        "The merge base {bad_hex} is new.\nThe property has changed between {bad_hex} and [{good_hex}]."
                    );
                } else {
                    eprintln!(
                        "The merge base {bad_hex} is {}.\nThis means the first '{}' commit is between {bad_hex} and [{good_hex}].",
                        terms.bad, terms.good
                    );
                }
                return Ok(BISECT_MERGE_BASE_CHECK);
            }
            eprintln!(
                "Some {} revs are not ancestors of the {} rev.\ngit bisect cannot work properly in this case.\nMaybe you mistook {} and {} revs?",
                terms.good, terms.bad, terms.good, terms.bad
            );
            return Ok(BISECT_FAILED);
        } else if goods.contains(base) {
            continue;
        } else if skipped.contains(base) {
            let good_hex = join_oids_hex(goods);
            eprintln!(
                "warning: the merge base between {} and [{good_hex}] must be skipped.\nSo we cannot be sure the first {} commit is between {} and {}.\nWe continue anyway.",
                bad.to_hex(),
                terms.bad,
                base.to_hex(),
                bad.to_hex()
            );
        } else {
            writeln!(out, "Bisecting: a merge base must be tested")?;
            let res = bisect_checkout(repo, base, no_checkout, out)?;
            if res == BISECT_OK {
                return Ok(BISECT_INTERNAL_SUCCESS_MERGE_BASE);
            }
            return Ok(res);
        }
    }
    Ok(BISECT_OK)
}

/// Upstream `check_good_are_ancestors_of_bad`.
fn check_good_are_ancestors_of_bad(
    repo: &BisectRepo,
    terms: &BisectTerms,
    bad: Option<&ObjectId>,
    goods: &[ObjectId],
    skipped: &HashSet<ObjectId>,
    no_checkout: bool,
    out: &mut dyn Write,
) -> Result<i32> {
    let Some(bad) = bad else {
        eprintln!("error: a {} revision is needed", terms.bad);
        return Ok(BISECT_FAILED);
    };
    if repo.state_path("BISECT_ANCESTORS_OK").is_file() {
        return Ok(BISECT_OK);
    }
    if goods.is_empty() {
        return Ok(BISECT_OK);
    }
    let db = repo.db();
    let mut all_ancestors = true;
    for good in goods {
        if !sley_rev::is_ancestor(&repo.git_dir, repo.format, &db, good, bad)? {
            all_ancestors = false;
            break;
        }
    }
    let res = if !all_ancestors {
        check_merge_bases(repo, terms, bad, goods, skipped, no_checkout, out)?
    } else {
        BISECT_OK
    };
    if res == BISECT_OK {
        let _ = fs::write(repo.state_path("BISECT_ANCESTORS_OK"), b"");
    }
    Ok(res)
}

/// The core next-step computation (upstream `bisect_next_all`).
fn bisect_next_all(repo: &BisectRepo, terms: &BisectTerms, out: &mut dyn Write) -> Result<i32> {
    let no_checkout = repo.no_checkout();
    let first_parent = repo.first_parent_mode();
    let bad = bisect_bad_oid(repo, terms)?;
    let goods = bisect_good_oids(repo, terms)?;
    let skipped: HashSet<ObjectId> = bisect_skip_oids(repo)?.into_iter().collect();
    let find_all = !skipped.is_empty();

    let res = check_good_are_ancestors_of_bad(
        repo,
        terms,
        bad.as_ref(),
        &goods,
        &skipped,
        no_checkout,
        out,
    )?;
    if res != BISECT_OK {
        return Ok(res);
    }
    let bad = bad.expect("checked by check_good_are_ancestors_of_bad");

    // Candidate list: `bad ^goods` (newest-first), restricted by BISECT_NAMES.
    let candidates = bisect_candidate_records(repo, &bad, &goods, first_parent)?;
    let db = repo.db();
    let kept: Vec<sley_rev::CommitRecord> = match bisect_pathspec(repo)? {
        Some(pathspec) => sley_rev::simplify_history(
            &db,
            repo.format,
            candidates.clone(),
            &pathspec,
            sley_rev::SimplifyOptions {
                full_history: false,
                first_parent,
                ..Default::default()
            },
        )?,
        None => candidates.clone(),
    };

    if kept.is_empty() && candidates.is_empty() {
        // Nothing reachable at all: bad was also good.
        writeln!(
            out,
            "{} was both {} and {}",
            bad.to_hex(),
            terms.good,
            terms.bad
        )?;
        return Ok(BISECT_FAILED);
    }
    if kept.is_empty() {
        eprintln!("No testable commit found.\nMaybe you started with bad path arguments?");
        return Ok(BISECT_NO_TESTABLE_COMMIT);
    }

    // Oldest-first list with intra-set parent adjacency.
    let mut list: Vec<&sley_rev::CommitRecord> = kept.iter().collect();
    list.reverse();
    let oids: Vec<ObjectId> = list.iter().map(|record| record.oid).collect();
    let index_by_oid: HashMap<ObjectId, usize> = oids
        .iter()
        .enumerate()
        .map(|(idx, oid)| (*oid, idx))
        .collect();
    let parents: Vec<Vec<usize>> = list
        .iter()
        .map(|record| {
            let mut adjacent = Vec::new();
            for parent in &record.parents {
                if let Some(&pidx) = index_by_oid.get(parent) {
                    adjacent.push(pidx);
                }
                if first_parent {
                    break;
                }
            }
            adjacent
        })
        .collect();

    let all = oids.len();
    let bisection = do_find_bisection(&oids, &parents, find_all);
    let reaches = bisection.reaches;

    let (bisect_rev, tried): (Option<ObjectId>, Vec<ObjectId>) =
        match managed_skipped(&bisection.picks, &oids, &skipped, &bad) {
            SkipFilter::Clean(idx) => (Some(oids[idx]), Vec::new()),
            SkipFilter::Skipped { pick, tried } => (
                pick.map(|idx| oids[idx]),
                tried.into_iter().map(|idx| oids[idx]).collect(),
            ),
        };

    let Some(bisect_rev) = bisect_rev else {
        let res = error_if_skipped_commits(repo, terms, &tried, None, out)?;
        if res != BISECT_OK {
            return Ok(res);
        }
        writeln!(
            out,
            "{} was both {} and {}",
            bad.to_hex(),
            terms.good,
            terms.bad
        )?;
        return Ok(BISECT_FAILED);
    };

    if bisect_rev == bad {
        let res = error_if_skipped_commits(repo, terms, &tried, Some(&bad), out)?;
        if res != BISECT_OK {
            return Ok(res);
        }
        writeln!(
            out,
            "{} is the first {} commit",
            bisect_rev.to_hex(),
            terms.bad
        )?;
        bisect_show_commit(repo, &bisect_rev, out)?;
        return Ok(BISECT_INTERNAL_SUCCESS_1ST_BAD_FOUND);
    }

    let nr = all - reaches - 1;
    let steps = estimate_bisect_steps(all);
    writeln!(
        out,
        "Bisecting: {nr} {} left to test after this (roughly {steps} {})",
        plural(nr, "revision", "revisions"),
        plural(steps, "step", "steps"),
    )?;

    bisect_checkout(repo, &bisect_rev, no_checkout, out)
}

/// Render the first-bad commit like `git show --stat --summary
/// --no-abbrev-commit --diff-merges=first-parent`.
fn bisect_show_commit(repo: &BisectRepo, oid: &ObjectId, out: &mut dyn Write) -> Result<()> {
    let db = repo.db();
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let commit = Commit::parse(repo.format, &object.body)?;
    writeln!(out, "commit {}", oid.to_hex())?;
    if commit.parents.len() > 1 {
        let short: Vec<String> = commit
            .parents
            .iter()
            .map(|parent| parent.to_hex()[..7].to_string())
            .collect();
        writeln!(out, "Merge: {}", short.join(" "))?;
    }
    writeln!(out, "Author: {}", commit_author_identity(&commit.author))?;
    writeln!(
        out,
        "Date:   {}",
        commit_identity_date(&commit.author, &DateMode::Default)
    )?;
    writeln!(out)?;
    for line in String::from_utf8_lossy(&commit.message).lines() {
        if line.is_empty() {
            writeln!(out)?;
        } else {
            writeln!(out, "    {line}")?;
        }
    }
    writeln!(out)?;

    // Diffstat against the first parent (or the empty tree for a root commit).
    let new_tree = commit.tree;
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
    let stat_entries = collect_diff_stat_entries(&entries, &db, None, false)?;
    write_diff_stat_materialized(
        out,
        &stat_entries,
        DiffStatOptions {
            compact_summary: false,
            stat_count: None,
            color: false,
        },
    )?;
    // `--summary`: creation/deletion/mode lines after the stat.
    for entry in &entries {
        match entry.status {
            sley_diff_merge::NameStatus::Added => writeln!(
                out,
                " create mode {:o} {}",
                entry.new_mode.unwrap_or(0o100644),
                String::from_utf8_lossy(&entry.path)
            )?,
            sley_diff_merge::NameStatus::Deleted => writeln!(
                out,
                " delete mode {:o} {}",
                entry.old_mode.unwrap_or(0o100644),
                String::from_utf8_lossy(&entry.path)
            )?,
            _ => {}
        }
    }
    Ok(())
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}
