//! Engine-level parity harness: compare [`sley`] library output against oracle git.
//!
//! Integration tests in `crates/sley/tests/parity/` use this module to exercise
//! [`sley::Repository`] (and related library APIs) directly while diffing
//! stdout, stderr, exit codes, and optional on-disk files against upstream git
//! run in a hermetic environment (see [`crate::hermetic_git_command`]).
//!
//! # Environment
//!
//! * **Oracle git:** [`crate::oracle_git`] (requires git 2.55.x or `SLEY_TEST_GIT`).
//! * **Hermetic config:** oracle subprocesses use [`crate::HERMETIC_GIT_CONFIG_PATH`]
//!   for global/system config so host `~/.gitconfig` does not skew comparisons.
//! * **Identity:** setup helpers that create commits use
//!   [`crate::hermetic_git_command_with_identity`].

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{hermetic_git_command, hermetic_git_command_with_identity, oracle_git};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Captured stdout, stderr, exit code, and optional file snapshots from one side
/// of a parity comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub files: HashMap<PathBuf, Vec<u8>>,
}

impl EngineOutput {
    /// Successful outcome with stdout only (exit 0, empty stderr).
    pub fn stdout(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            exit_code: 0,
            stdout: bytes.into(),
            stderr: Vec::new(),
            files: HashMap::new(),
        }
    }

    /// Failed outcome mirroring a subprocess exit.
    pub fn failure(exit_code: i32, stderr: impl Into<Vec<u8>>) -> Self {
        Self {
            exit_code,
            stdout: Vec::new(),
            stderr: stderr.into(),
            files: HashMap::new(),
        }
    }

    pub fn with_files(mut self, files: HashMap<PathBuf, Vec<u8>>) -> Self {
        self.files = files;
        self
    }
}

/// Hermetic temporary repository root for engine parity tests.
///
/// Created via [`hermetic_repo`]; removed on drop. Repository setup is typically
/// performed through oracle git so both sides start from identical on-disk state.
pub struct HermeticRepo {
    root: PathBuf,
}

impl HermeticRepo {
    /// Create an empty temp directory named `sley-engine-parity-{name}-…`.
    pub fn new(name: &str) -> Self {
        let root = unique_temp_dir(&format!("sley-engine-parity-{name}"));
        fs::create_dir_all(&root).expect("create hermetic repo root");
        Self { root }
    }

    /// The repository worktree root (or bare git dir when used as such).
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Initialize a non-bare repository with `main` as the default branch.
    pub fn init_default(&self) {
        self.oracle_ok(&["init", "-q", "-b", "main"]);
    }

    /// Initialize a bare repository at `{root}/{name}` and return its path.
    pub fn init_bare(&self, name: &str) -> PathBuf {
        let bare = self.root.join(name);
        self.oracle_ok_in(&self.path(), &["init", "-q", "--bare", name, "-b", "main"]);
        bare
    }

    /// Write a worktree-relative file.
    pub fn write_file(&self, rel: &str, bytes: &[u8]) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, bytes).expect("write file");
    }

    /// Create nested worktree directory `{root}/{components…}`.
    pub fn mkdir(&self, rel: &str) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(&path).expect("create directory");
        path
    }

    /// Seed a mixed worktree/index fixture for `update-index` parity tests.
    ///
    /// After this call the index contains `one.txt` and `keep.txt`; `one.txt` is
    /// removed from disk, `keep.txt` is modified, and `new.txt` / `z.txt` exist
    /// unstaged.
    pub fn seed_update_index_fixture(&self) {
        self.init_default();
        self.write_file("one.txt", b"one");
        self.write_file("keep.txt", b"keep");
        self.oracle_ok(&["add", "one.txt", "keep.txt"]);
        self.write_file("keep.txt", b"changed");
        let _ = fs::remove_file(self.root.join("one.txt"));
        self.write_file("new.txt", b"new");
        self.write_file("z.txt", b"z");
    }

    /// Capture `git ls-files --stage` stdout for this fixture.
    pub fn index_stage_output(&self) -> EngineOutput {
        self.oracle(&["ls-files", "--stage"])
    }

    /// Write an empty `shallow` marker under `git_dir`.
    pub fn write_shallow_marker(&self, git_dir: &Path) {
        fs::write(git_dir.join("shallow"), b"").expect("write shallow marker");
    }

    /// Stage and commit `paths` with deterministic identity (for cat-file fixtures).
    pub fn commit_paths(&self, message: &str, paths: &[&str]) {
        for path in paths {
            self.oracle_ok(&["add", path]);
        }
        self.oracle_ok_with_identity(&[
            "commit",
            "-m",
            message,
            "-q",
        ]);
    }

    /// Run oracle git in this repo's root, returning full output.
    pub fn oracle(&self, args: &[&str]) -> EngineOutput {
        self.oracle_in(self.path(), args)
    }

    /// Run oracle git in `cwd`, returning full output.
    pub fn oracle_in(&self, cwd: &Path, args: &[&str]) -> EngineOutput {
        self.oracle_with_env(cwd, args, &[])
    }

    /// Run oracle git in `cwd` with an environment overlay.
    pub fn oracle_with_env(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> EngineOutput {
        output_to_engine(run_oracle_command(cwd, args, env, &[]))
    }

    /// Like [`Self::oracle`] but panics unless exit status is success.
    pub fn oracle_ok(&self, args: &[&str]) -> Vec<u8> {
        self.oracle_ok_in(self.path(), args)
    }

    /// Like [`Self::oracle_in`] but panics unless exit status is success.
    pub fn oracle_ok_in(&self, cwd: &Path, args: &[&str]) -> Vec<u8> {
        let output = self.oracle_in(cwd, args);
        assert!(
            output.exit_code == 0,
            "oracle git {args:?} failed with status {} in {}\nstdout:\n{}\nstderr:\n{}",
            output.exit_code,
            cwd.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    /// Run oracle git with deterministic author/committer identity.
    pub fn oracle_ok_with_identity(&self, args: &[&str]) -> Vec<u8> {
        let output = output_to_engine(run_oracle_command_with_identity(
            self.path(),
            args,
            &[],
            &[],
        ));
        assert!(
            output.exit_code == 0,
            "oracle git {args:?} failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.exit_code,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
}

impl Drop for HermeticRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Create a [`HermeticRepo`] temp fixture.
pub fn hermetic_repo(name: &str) -> HermeticRepo {
    HermeticRepo::new(name)
}

/// Assert two byte slices are equal, with a readable diff context.
pub fn assert_bytes_eq(actual: &[u8], expected: &[u8], context: &str) {
    assert_eq!(
        actual, expected,
        "{context}\nactual stdout:\n{}\nexpected stdout:\n{}",
        String::from_utf8_lossy(actual),
        String::from_utf8_lossy(expected)
    );
}

/// Assert stdout bytes match between two [`EngineOutput`] values.
pub fn assert_stdout_eq(actual: &EngineOutput, expected: &EngineOutput, context: &str) {
    assert_bytes_eq(&actual.stdout, &expected.stdout, context);
}

/// Default comparison: exit code, stdout, stderr, and tracked files.
pub fn assert_engine_parity(case_name: &str, sley: &EngineOutput, oracle: &EngineOutput) {
    assert_eq!(
        sley.exit_code, oracle.exit_code,
        "{case_name}: exit code differed\nsley stderr:\n{}\noracle stderr:\n{}",
        String::from_utf8_lossy(&sley.stderr),
        String::from_utf8_lossy(&oracle.stderr)
    );
    assert_stdout_eq(
        sley,
        oracle,
        &format!("{case_name}: stdout differed"),
    );
    assert_eq!(
        sley.stderr, oracle.stderr,
        "{case_name}: stderr differed\nsley stderr:\n{}\noracle stderr:\n{}",
        String::from_utf8_lossy(&sley.stderr),
        String::from_utf8_lossy(&oracle.stderr)
    );
    assert_eq!(sley.files, oracle.files, "{case_name}: file snapshot differed");
}

/// One engine parity scenario: shared setup, library runner, oracle runner, compare.
pub struct EngineParityCase<'a> {
    pub name: &'a str,
}

impl<'a> EngineParityCase<'a> {
    pub fn new(name: &'a str) -> Self {
        Self { name }
    }

    /// Run `setup`, then `run_sley` and `run_oracle`, and apply [`assert_engine_parity`].
    pub fn run(
        self,
        setup: impl FnOnce(&mut HermeticRepo),
        run_sley: impl FnOnce(&HermeticRepo) -> EngineOutput,
        run_oracle: impl FnOnce(&HermeticRepo) -> EngineOutput,
    ) {
        let name = self.name;
        self.run_with_compare(setup, run_sley, run_oracle, move |sley, oracle| {
            assert_engine_parity(name, sley, oracle);
        });
    }

    /// Run `setup`, then `run_sley` and `run_oracle`, and apply a custom `compare`.
    pub fn run_with_compare(
        self,
        setup: impl FnOnce(&mut HermeticRepo),
        run_sley: impl FnOnce(&HermeticRepo) -> EngineOutput,
        run_oracle: impl FnOnce(&HermeticRepo) -> EngineOutput,
        compare: impl FnOnce(&EngineOutput, &EngineOutput),
    ) {
        let mut fixture = hermetic_repo(self.name);
        setup(&mut fixture);
        let sley = run_sley(&fixture);
        let oracle = run_oracle(&fixture);
        compare(&sley, &oracle);
    }
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn output_to_engine(output: Output) -> EngineOutput {
    EngineOutput {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
        files: HashMap::new(),
    }
}

fn run_oracle_command(cwd: &Path, args: &[&str], env: &[(&str, &str)], stdin: &[u8]) -> Output {
    let mut command = hermetic_git_command(oracle_git());
    apply_oracle_overrides(&mut command, cwd, args, env);
    spawn_and_wait(&mut command, stdin)
}

fn run_oracle_command_with_identity(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: &[u8],
) -> Output {
    let mut command = hermetic_git_command_with_identity(oracle_git());
    apply_oracle_overrides(&mut command, cwd, args, env);
    spawn_and_wait(&mut command, stdin)
}

fn apply_oracle_overrides(command: &mut Command, cwd: &Path, args: &[&str], env: &[(&str, &str)]) {
    command.current_dir(cwd).args(args);
    for (key, value) in env {
        command.env(key, value);
    }
}

fn spawn_and_wait(command: &mut Command, stdin: &[u8]) -> Output {
    if stdin.is_empty() {
        return command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap_or_else(|err| panic!("failed to run oracle git: {err}"));
    }

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn oracle git: {err}"));
    if let Some(mut pipe) = child.stdin.take() {
        crate::write_stdin_tolerating_early_exit(&mut pipe, stdin);
    }
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for oracle git: {err}"))
}

/// Format a git-style boolean line (`true\n` / `false\n`).
pub fn git_bool_line(value: bool) -> Vec<u8> {
    if value {
        b"true\n".to_vec()
    } else {
        b"false\n".to_vec()
    }
}

/// Format a git-style object id line (`{hex}\n`).
pub fn git_oid_line(hex: impl AsRef<[u8]>) -> Vec<u8> {
    let mut line = hex.as_ref().to_vec();
    line.push(b'\n');
    line
}

/// Format a git-style config value line (`{value}\n`), or empty stdout when unset.
pub fn git_config_line(value: Option<&str>) -> Vec<u8> {
    match value {
        Some(value) => {
            let mut line = value.as_bytes().to_vec();
            line.push(b'\n');
            line
        }
        None => Vec::new(),
    }
}

/// Format multiple config values the way `git config --get-all` prints them.
pub fn git_config_get_all_lines(values: &[Option<&str>]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        if let Some(value) = value {
            out.extend_from_slice(value.as_bytes());
            out.push(b'\n');
        }
    }
    out
}

/// Format a git-style path line (`{path}\n`), used by `rev-parse --show-toplevel`.
pub fn git_path_line(path: impl AsRef<Path>) -> Vec<u8> {
    let path = fs::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf());
    let mut line = path.to_string_lossy().into_owned().into_bytes();
    line.push(b'\n');
    line
}

/// Format a git-style object size line (`{size}\n`), used by `cat-file -s`.
pub fn git_size_line(size: u64) -> Vec<u8> {
    let mut line = size.to_string().into_bytes();
    line.push(b'\n');
    line
}

/// Format a symbolic ref target line (`refs/heads/main\n`).
pub fn git_symbolic_ref_line(target: &str) -> Vec<u8> {
    let mut line = target.as_bytes().to_vec();
    line.push(b'\n');
    line
}

/// Format index entries the way `git ls-files --stage` prints them.
pub fn format_index_stage_lines(entries: &[sley_index::IndexEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        let stage = (entry.flags >> 12) & 0x3;
        out.extend_from_slice(format!("{:06o} {} {stage}\t", entry.mode, entry.oid).as_bytes());
        out.extend_from_slice(entry.path.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Build a `Command` for oracle git with hermetic defaults (for bespoke test helpers).
pub fn oracle_command(program: impl AsRef<OsStr>) -> Command {
    hermetic_git_command(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_bool_line_formats_like_rev_parse() {
        assert_eq!(git_bool_line(true), b"true\n");
        assert_eq!(git_bool_line(false), b"false\n");
    }

    #[test]
    fn git_config_get_all_joins_with_newlines() {
        assert_eq!(
            git_config_get_all_lines(&[Some("a"), Some("b")]),
            b"a\nb\n"
        );
    }
}