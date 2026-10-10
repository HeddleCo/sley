//! Hostile-tree checkout tests (HeddleCo/sley#247).
//!
//! A remote controls every tree entry name it sends. Git refuses, before
//! touching the worktree, any path that names the repository's `.git`
//! directory on *some* filesystem: `.GIT` (case-insensitive filesystems),
//! `.git.` / `.git ` / `.git::$INDEX_ALLOCATION` / `GIT~1` (NTFS), and `.git`
//! with code points HFS+ ignores (`.g\u{200c}it`). These tests drive sley's
//! checkout entry points with such trees and require the same refusal, with
//! nothing written to the worktree, to `.git`, or outside the worktree.

use sley_config::GitConfig;
use sley_core::{BString, ObjectFormat, ObjectId};
use sley_object::{EncodedObject, ObjectType, Tree, TreeEntry};
use sley_odb::{FileObjectDatabase, ObjectWriter};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const FORMAT: ObjectFormat = ObjectFormat::Sha1;
const PAYLOAD: &[u8] = b"#!/bin/sh\necho pwned > \"$GIT_DIR/../PWNED\"\n";

struct Repo {
    _base: tempfile::TempDir,
    /// Parent of the worktree; anything written here escaped the worktree.
    outside: PathBuf,
    root: PathBuf,
    git_dir: PathBuf,
    db: FileObjectDatabase,
}

impl Repo {
    fn new() -> Self {
        let base = tempfile::tempdir().expect("create temporary directory");
        let outside = base.path().to_path_buf();
        let root = outside.join("worktree");
        let git_dir = root.join(".git");
        for dir in ["objects", "refs/heads", "refs/tags", "hooks", "info"] {
            fs::create_dir_all(git_dir.join(dir)).expect("create git directory");
        }
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("write HEAD");
        fs::write(
            git_dir.join("config"),
            b"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .expect("write config");
        let db = FileObjectDatabase::from_git_dir(&git_dir, FORMAT);
        Self {
            _base: base,
            outside,
            root,
            git_dir,
            db,
        }
    }

    fn blob(&self, body: &[u8]) -> ObjectId {
        self.db
            .write_object(EncodedObject::new(ObjectType::Blob, body.to_vec()))
            .expect("write blob")
    }

    /// Build a commit whose tree holds exactly `files` (path, mode, body).
    /// Paths are split on `/` only, so a component may contain `\`, `:` or
    /// any other byte a hostile remote can put in a tree entry name.
    fn commit(&self, files: &[(&[u8], u32, &[u8])]) -> ObjectId {
        #[derive(Default)]
        struct Dir {
            files: BTreeMap<Vec<u8>, (u32, ObjectId)>,
            dirs: BTreeMap<Vec<u8>, Dir>,
        }
        fn write_dir(db: &FileObjectDatabase, dir: &Dir) -> ObjectId {
            let mut entries = Vec::new();
            for (name, (mode, oid)) in &dir.files {
                entries.push((name.clone(), *mode, *oid));
            }
            for (name, child) in &dir.dirs {
                entries.push((name.clone(), 0o040000, write_dir(db, child)));
            }
            // Git tree order: compare names as if trees had a trailing '/'.
            entries.sort_by(|(left, left_mode, _), (right, right_mode, _)| {
                let key = |name: &Vec<u8>, mode: u32| {
                    let mut key = name.clone();
                    if mode == 0o040000 {
                        key.push(b'/');
                    }
                    key
                };
                key(left, *left_mode).cmp(&key(right, *right_mode))
            });
            let entries = entries
                .into_iter()
                .map(|(name, mode, oid)| TreeEntry {
                    mode,
                    name: BString::from(name),
                    oid,
                })
                .collect();
            db.write_object(EncodedObject::new(
                ObjectType::Tree,
                Tree { entries }.write(),
            ))
            .expect("write tree")
        }

        let mut root = Dir::default();
        for (path, mode, body) in files {
            let components: Vec<&[u8]> = path.split(|byte| *byte == b'/').collect();
            let (leaf, parents) = components.split_last().expect("non-empty path");
            let mut dir = &mut root;
            for component in parents {
                dir = dir.dirs.entry(component.to_vec()).or_default();
            }
            dir.files.insert(leaf.to_vec(), (*mode, self.blob(body)));
        }
        let tree = write_dir(&self.db, &root);
        let body = format!(
            "tree {tree}\nauthor A U Thor <author@example.com> 0 +0000\n\
             committer C O Mitter <committer@example.com> 0 +0000\n\nhostile\n"
        );
        self.db
            .write_object(EncodedObject::new(ObjectType::Commit, body.into_bytes()))
            .expect("write commit")
    }

    fn set_branch(&self, branch: &str, oid: &ObjectId) {
        fs::write(
            self.git_dir.join("refs/heads").join(branch),
            format!("{oid}\n"),
        )
        .expect("write branch ref");
    }

    /// Every file and directory under `dir`, relative to it, excluding `.git`.
    fn listing(dir: &Path, skip_git: bool) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(entries) = fs::read_dir(&current) else {
                continue;
            };
            for entry in entries {
                let entry = entry.expect("read dir entry");
                let path = entry.path();
                if skip_git && path == dir.join(".git") {
                    continue;
                }
                let relative = path
                    .strip_prefix(dir)
                    .expect("under dir")
                    .to_string_lossy()
                    .into_owned();
                #[cfg(windows)]
                let relative = relative.replace('\\', "/");
                out.push(relative);
                let file_type = entry.file_type().expect("file type");
                if file_type.is_dir() {
                    stack.push(path);
                }
            }
        }
        out.sort();
        out
    }

    fn git_dir_listing(&self) -> Vec<String> {
        Self::listing(&self.git_dir, false)
    }

    fn worktree_listing(&self) -> Vec<String> {
        Self::listing(&self.root, true)
    }

    fn outside_listing(&self) -> Vec<String> {
        let mut entries = Self::listing(&self.outside, false);
        entries.retain(|entry| entry != "worktree" && !entry.starts_with("worktree/"));
        entries
    }
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    CheckoutBranch,
    CheckoutDetached,
    Reset,
}

const ENTRY_POINTS: [Entry; 3] = [Entry::CheckoutBranch, Entry::CheckoutDetached, Entry::Reset];

fn run_checkout(repo: &Repo, entry: Entry, commit: &ObjectId) -> sley_core::Result<()> {
    let config = GitConfig::default();
    match entry {
        Entry::CheckoutBranch => {
            repo.set_branch("hostile", commit);
            sley_worktree::checkout_branch_filtered(
                None,
                &repo.root,
                &repo.git_dir,
                FORMAT,
                "hostile",
                b"C O Mitter <committer@example.com> 0 +0000".to_vec(),
                &config,
            )
            .map(|_| ())
        }
        Entry::CheckoutDetached => sley_worktree::checkout_detached_filtered(
            None,
            &repo.root,
            &repo.git_dir,
            FORMAT,
            commit,
            b"C O Mitter <committer@example.com> 0 +0000".to_vec(),
            b"checkout: hostile".to_vec(),
            &config,
        )
        .map(|_| ()),
        Entry::Reset => sley_worktree::reset_index_and_worktree_to_commit(
            None,
            &repo.root,
            &repo.git_dir,
            FORMAT,
            commit,
        )
        .map(|_| ()),
    }
}

/// Hostile paths, each a write into `.git` on some filesystem git protects by
/// default (NTFS everywhere, HFS+ via `core.protectHFS`), or out of the
/// worktree. Mode is the tree entry mode of the leaf.
fn hostile_corpus() -> Vec<(Vec<u8>, u32)> {
    let hook = 0o100755;
    let link = 0o120000;
    let mut corpus: Vec<(Vec<u8>, u32)> = [
        // The literal name and case aliases (case-insensitive filesystems).
        ".git/hooks/post-checkout",
        ".GIT/hooks/post-checkout",
        ".Git/hooks/post-checkout",
        ".gIT/config",
        "sub/.GIT/hooks/post-checkout",
        // NTFS: trailing dots and spaces are stripped.
        ".git./hooks/post-checkout",
        ".git /hooks/post-checkout",
        ".git . ./hooks/post-checkout",
        ".GIT../hooks/post-checkout",
        // NTFS 8.3 short name.
        "GIT~1/hooks/post-checkout",
        "git~1/hooks/post-checkout",
        "Git~1./hooks/post-checkout",
        "sub/git~1/config",
        // NTFS alternate data streams, including the directory stream.
        ".git::$INDEX_ALLOCATION/hooks/post-checkout",
        ".GIT::$INDEX_ALLOCATION/config",
        "git~1::$INDEX_ALLOCATION/config",
        ".git:stream",
        // NTFS: backslash is a directory separator inside one tree name.
        ".git\\hooks\\post-checkout",
        "a\\.git\\hooks\\post-checkout",
        "a\\GIT~1\\config",
        // Path traversal and degenerate components.
        "../escaped",
        "sub/../../escaped",
        "./dot-component",
    ]
    .into_iter()
    .map(|path| (path.as_bytes().to_vec(), hook))
    .collect();
    // HFS+ ignorable code points spliced into `.git`.
    for path in [
        ".g\u{200c}it/hooks/post-checkout",
        ".gi\u{200d}t/hooks/post-checkout",
        "\u{200e}.git/hooks/post-checkout",
        ".GI\u{200f}T/config",
        ".g\u{202a}it/config",
        ".g\u{202e}it/config",
        ".g\u{206a}it/config",
        ".g\u{206f}it/config",
        ".git\u{feff}/hooks/post-checkout",
        "sub/.g\u{200c}it/config",
    ] {
        corpus.push((path.as_bytes().to_vec(), hook));
    }
    // `.gitmodules` may not be a symlink under any alias (CVE-2018-11235).
    for path in [
        ".gitmodules",
        ".GITMODULES",
        ".gitmodules.",
        ".gitmodules ",
        ".gitmodules::$DATA",
        "gitmod~1",
        "GITMOD~4",
        "gi7eba~1",
        ".g\u{200c}itmodules",
        "sub/.gitmodules",
    ] {
        corpus.push((path.as_bytes().to_vec(), link));
    }
    corpus
}

fn assert_refused_without_side_effects(path: &[u8], mode: u32, entry: Entry) {
    let repo = Repo::new();
    let git_before = repo.git_dir_listing();
    let body: &[u8] = if mode == 0o120000 { b".git" } else { PAYLOAD };
    // A legitimate sibling proves the refusal happens before any write.
    let commit = repo.commit(&[(b"README", 0o100644, b"hello\n"), (path, mode, body)]);
    let result = run_checkout(&repo, entry, &commit);
    let shown = String::from_utf8_lossy(path);
    assert!(
        result.is_err(),
        "{entry:?}: checkout of hostile path {shown:?} (mode {mode:o}) must be refused"
    );
    assert_eq!(
        repo.worktree_listing(),
        Vec::<String>::new(),
        "{entry:?}: refusing {shown:?} must happen before anything is written"
    );
    assert!(
        !repo.git_dir.join("hooks/post-checkout").exists(),
        "{entry:?}: {shown:?} planted a hook"
    );
    let mut git_after = repo.git_dir_listing();
    // A refused checkout may still have written ref/index bookkeeping (git
    // writes nothing either; sley must at least not add hook/config files).
    git_after.retain(|entry| !git_before.contains(entry));
    git_after.retain(|entry| entry.starts_with("hooks") || entry == "config");
    assert_eq!(
        git_after,
        Vec::<String>::new(),
        "{entry:?}: {shown:?} wrote into .git"
    );
    assert_eq!(
        repo.outside_listing(),
        Vec::<String>::new(),
        "{entry:?}: {shown:?} wrote outside the worktree"
    );
}

#[test]
fn hostile_dotgit_aliases_are_refused_by_every_checkout_entry_point() {
    let mut failures = Vec::new();
    for entry in ENTRY_POINTS {
        for (path, mode) in hostile_corpus() {
            let outcome = std::panic::catch_unwind(|| {
                assert_refused_without_side_effects(&path, mode, entry);
            });
            if let Err(panic) = outcome {
                let message = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_string()))
                    .unwrap_or_default();
                failures.push(message);
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} hostile checkouts were not refused cleanly:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn ordinary_dotfiles_still_check_out() {
    for entry in ENTRY_POINTS {
        let repo = Repo::new();
        let files: Vec<(&[u8], u32, &[u8])> = vec![
            (b".github/workflows/ci.yml", 0o100644, b"on: push\n"),
            (b".gitignore", 0o100644, b"target/\n"),
            (b".gitattributes", 0o100644, b"*.rs diff=rust\n"),
            (b".gitmodules", 0o100644, b"# no submodules\n"),
            (b".git-blame-ignore-revs", 0o100644, b"\n"),
            (b".gitkeep", 0o100644, b""),
            (b".gitx/file", 0o100644, b"x\n"),
            (b"x.git/file", 0o100644, b"x\n"),
            (b"git/file", 0o100644, b"x\n"),
            (b"git~2/file", 0o100644, b"x\n"),
            (b".heddle-notes/file", 0o100644, b"x\n"),
            (b"sub/.heddle/file", 0o100644, b"x\n"),
            (b"..dots", 0o100644, b"x\n"),
            (b"link", 0o120000, b".gitignore"),
            (b"sub/.gitignore", 0o100644, b"*.o\n"),
        ];
        let commit = repo.commit(&files);
        run_checkout(&repo, entry, &commit)
            .unwrap_or_else(|error| panic!("{entry:?}: ordinary checkout failed: {error}"));
        for (path, mode, body) in &files {
            let on_disk = repo.root.join(String::from_utf8_lossy(path).as_ref());
            if *mode == 0o120000 {
                #[cfg(unix)]
                assert_eq!(
                    fs::read_link(&on_disk).expect("read symlink"),
                    PathBuf::from(String::from_utf8_lossy(body).as_ref()),
                    "{entry:?}: {}",
                    on_disk.display()
                );
            } else {
                assert_eq!(
                    fs::read(&on_disk).expect("read checked-out file"),
                    *body,
                    "{entry:?}: {}",
                    on_disk.display()
                );
            }
        }
    }
}

/// The Linux-exploitable case: on a case-sensitive filesystem the literal
/// `.git` component is the only alias that resolves into the repository, and
/// a tree carrying it must never plant a hook, whatever the entry point.
#[test]
fn literal_dotgit_tree_never_plants_a_hook() {
    let mut planted = Vec::new();
    for entry in ENTRY_POINTS {
        let repo = Repo::new();
        let commit = repo.commit(&[
            (b"README", 0o100644, b"hello\n"),
            (b".git/hooks/post-checkout", 0o100755, PAYLOAD),
        ]);
        let result = run_checkout(&repo, entry, &commit);
        if repo.git_dir.join("hooks/post-checkout").exists() {
            planted.push(format!("{entry:?} (checkout returned {result:?})"));
        }
    }
    assert!(planted.is_empty(), "hook planted via: {planted:?}");
}

fn run_checkout_with_policy(
    repo: &Repo,
    entry: Entry,
    commit: &ObjectId,
    policy: &sley_worktree::WorktreePathPolicy,
) -> sley_core::Result<()> {
    let config = GitConfig::read(repo.git_dir.join("config")).expect("repo config");
    match entry {
        Entry::CheckoutBranch => {
            repo.set_branch("hostile", commit);
            sley_worktree::checkout_branch_filtered_with_path_policy(
                None,
                &repo.root,
                &repo.git_dir,
                FORMAT,
                "hostile",
                b"C O Mitter <committer@example.com> 0 +0000".to_vec(),
                &config,
                policy,
            )
            .map(|_| ())
        }
        Entry::CheckoutDetached => sley_worktree::checkout_detached_filtered_with_path_policy(
            None,
            &repo.root,
            &repo.git_dir,
            FORMAT,
            commit,
            b"C O Mitter <committer@example.com> 0 +0000".to_vec(),
            b"checkout: hostile".to_vec(),
            &config,
            policy,
        )
        .map(|_| ()),
        Entry::Reset => sley_worktree::reset_index_and_worktree_to_commit_with_path_policy(
            None,
            &repo.root,
            &repo.git_dir,
            FORMAT,
            commit,
            policy,
        )
        .map(|_| ()),
    }
}

/// heddle's contract: a root `.heddle` (under any `.git`-style alias) is
/// refused when the caller reserves it, while `.heddle` below the root and
/// all of git's own rules still apply.
#[test]
fn caller_reserved_root_name_is_refused_only_at_the_root() {
    let policy = sley_worktree::WorktreePathPolicy::default().reserve_root_name(".heddle");
    for entry in ENTRY_POINTS {
        for hostile in [
            &b".heddle/config"[..],
            b".HEDDLE/objects/x",
            b".heddle./config",
            b"HEDDLE~1/config",
            b".heddle::$INDEX_ALLOCATION/config",
            ".hed\u{200c}dle/config".as_bytes(),
            b".GIT/hooks/post-checkout",
        ] {
            let repo = Repo::new();
            let commit =
                repo.commit(&[(b"README", 0o100644, b"hi\n"), (hostile, 0o100644, PAYLOAD)]);
            let result = run_checkout_with_policy(&repo, entry, &commit, &policy);
            let shown = String::from_utf8_lossy(hostile);
            assert!(result.is_err(), "{entry:?}: {shown:?} must be refused");
            assert_eq!(
                repo.worktree_listing(),
                Vec::<String>::new(),
                "{entry:?}: {shown:?}"
            );
        }

        let repo = Repo::new();
        let commit = repo.commit(&[
            (b"docs/.heddle/notes", 0o100644, b"nested is content\n"),
            (b".heddle-ignore", 0o100644, b"x\n"),
        ]);
        run_checkout_with_policy(&repo, entry, &commit, &policy)
            .unwrap_or_else(|error| panic!("{entry:?}: nested .heddle refused: {error}"));
        assert_eq!(
            fs::read(repo.root.join("docs/.heddle/notes")).expect("nested .heddle"),
            b"nested is content\n"
        );

        // Without the reservation a root `.heddle` is ordinary content: the
        // name is reserved by the caller, not by git.
        let repo = Repo::new();
        let commit = repo.commit(&[(b".heddle/config", 0o100644, b"x\n")]);
        run_checkout(&repo, entry, &commit)
            .unwrap_or_else(|error| panic!("{entry:?}: unreserved .heddle refused: {error}"));
    }
}

/// The explicit policy must not affect subsequent calls.
#[test]
fn path_policy_is_scoped_to_its_call() {
    let policy = sley_worktree::WorktreePathPolicy::default().reserve_root_name(".heddle");
    let repo = Repo::new();
    let commit = repo.commit(&[(b".heddle/config", 0o100644, b"x\n")]);
    assert!(run_checkout_with_policy(&repo, Entry::CheckoutDetached, &commit, &policy).is_err());
    run_checkout(&repo, Entry::CheckoutDetached, &commit).expect("policy leaked past its call");
}

/// `core.protectNTFS=false` / `core.protectHFS=false` relax exactly what they
/// relax in git, and nothing more.
#[test]
fn protection_switches_follow_repository_config() {
    let repo = Repo::new();
    fs::write(
        repo.git_dir.join("config"),
        b"[core]\n\trepositoryformatversion = 0\n\tbare = false\n\
          \tprotectNTFS = false\n\tprotectHFS = false\n",
    )
    .expect("write config");
    let commit = repo.commit(&[
        (b"git~1/file", 0o100644, b"x\n"),
        (".g\u{200c}it/file".as_bytes(), 0o100644, b"x\n"),
    ]);
    run_checkout(&repo, Entry::Reset, &commit).expect("relaxed protections allow aliases");
    assert!(repo.root.join("git~1/file").is_file());

    let commit = repo.commit(&[(b".GIT/file", 0o100644, b"x\n")]);
    assert!(
        run_checkout(&repo, Entry::Reset, &commit).is_err(),
        ".GIT is refused regardless of protectNTFS/protectHFS"
    );
}

fn oracle_git() -> Option<std::process::Command> {
    static GIT_AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let available = *GIT_AVAILABLE.get_or_init(|| {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|probe| probe.status.success())
    });
    if !available {
        assert!(
            std::env::var_os("CI").is_none(),
            "git is required for path-safety differential tests under CI"
        );
        return None;
    }
    let mut command = std::process::Command::new("git");
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["-c", "core.protectNTFS=true", "-c", "core.protectHFS=true"]);
    Some(command)
}

fn path_arg(path: &[u8]) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(path.to_vec())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8_lossy(path).into_owned().into()
    }
}

/// Differential check: sley's `verify_path` port agrees with real git's
/// (`update-index --cacheinfo` runs `verify_path`) on every hostile name
/// and on near-miss names git accepts, and a real `git checkout` refuses
/// every hostile tree sley refuses. Git's HFS protection is on by default
/// only on macOS; it is forced on here to match sley's default.
#[test]
fn verdicts_match_real_git() {
    let Some(_) = oracle_git() else {
        eprintln!("skipping: no git on PATH");
        return;
    };
    let mut corpus = hostile_corpus();
    for (path, mode) in [
        ("\\.git", 0o100644),
        ("gi~1/x", 0o100644),
        ("git~2/x", 0o100644),
        ("git~10/x", 0o100644),
        (".gitx/x", 0o100644),
        (".git.x", 0o100644),
        (".git-blame-ignore-revs", 0o100644),
        (".gitmodules", 0o100644),
        ("gitmod~1", 0o100644),
        ("gitmod~5", 0o120000),
        (".gitmodulesx", 0o120000),
        (".g\u{00ad}it/x", 0o100644),
        (".github/workflows/ci.yml", 0o100644),
        ("sub/.gitignore", 0o100644),
        ("..dots", 0o100644),
        ("a.git/x", 0o100644),
        ("git/x", 0o100644),
        (".heddle/config", 0o100644),
    ] {
        corpus.push((path.as_bytes().to_vec(), mode));
    }
    let generated = randomized_names(2_500);
    assert_eq!(generated.len(), 2_500);
    corpus.extend(generated);
    eprintln!(
        "path-safety oracle: {} names (2,500 generated), NTFS and HFS enabled",
        corpus.len()
    );
    let scratch = tempfile::tempdir().expect("scratch repository");
    let init = oracle_git()
        .expect("git")
        .args(["init", "-q"])
        .arg(scratch.path())
        .status()
        .expect("git init");
    assert!(init.success());
    let blob = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
    let policy = sley_worktree::WorktreePathPolicy::default();
    let mut disagreements = Vec::new();
    for (path, mode) in &corpus {
        // Each name gets an empty index; unrelated D/F conflicts must not
        // influence the oracle's path-validation verdict.
        let index_path = scratch.path().join("oracle-index");
        let _ = fs::remove_file(&index_path);
        let output = oracle_git()
            .expect("git")
            .env("GIT_INDEX_FILE", &index_path)
            .arg("-C")
            .arg(scratch.path())
            // The three-argument `--cacheinfo` form takes the path verbatim.
            .args(["update-index", "--add", "--cacheinfo"])
            .arg(format!("{mode:o}"))
            .arg(blob)
            .arg(path_arg(path))
            .output()
            .expect("git update-index");
        let git_accepts = output.status.success();
        let sley_accepts = policy.is_valid_path(path, *mode);
        if git_accepts != sley_accepts {
            disagreements.push(format!(
                "{:?} mode {mode:o}: git {} / sley {} ({})",
                String::from_utf8_lossy(path),
                if git_accepts { "accepts" } else { "refuses" },
                if sley_accepts { "accepts" } else { "refuses" },
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    assert!(
        disagreements.is_empty(),
        "verdicts differ:\n{}",
        disagreements.join("\n")
    );

    // End to end: real `git checkout` refuses the hostile trees too, and
    // writes nothing (the corpus is genuinely hostile, not sley-specific).
    for (path, mode) in hostile_corpus() {
        let repo = Repo::new();
        let body: &[u8] = if mode == 0o120000 { b".git" } else { PAYLOAD };
        let commit = repo.commit(&[(b"README", 0o100644, b"hello\n"), (&path, mode, body)]);
        let output = oracle_git()
            .expect("git")
            .arg("--git-dir")
            .arg(&repo.git_dir)
            .arg("--work-tree")
            .arg(&repo.root)
            .args(["checkout", "-q", "--detach"])
            .arg(commit.to_string())
            .output()
            .expect("git checkout");
        let shown = String::from_utf8_lossy(&path);
        assert!(!output.status.success(), "git checkout accepted {shown:?}");
        assert_eq!(
            repo.worktree_listing(),
            Vec::<String>::new(),
            "git wrote for {shown:?}"
        );
        assert!(!repo.git_dir.join("hooks/post-checkout").exists());
    }
}

#[test]
fn standalone_writer_enforces_configured_alias_rules() {
    for path in [
        b".git./hooks/x".as_slice(),
        b"GIT~1/hooks/x",
        ".g\u{200c}it/hooks/x".as_bytes(),
    ] {
        let repo = Repo::new();
        let result =
            sley_worktree::write_worktree_entry(None, &repo.root, path, 0o100644, b"content");
        assert!(
            matches!(result, Err(sley_core::GitError::InvalidPath(_))),
            "{path:?}: {result:?}"
        );
        assert!(repo.worktree_listing().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn reset_removal_treats_symlinked_parent_as_absent() {
    let repo = Repo::new();
    let first = repo.commit(&[(b"A/authorized_keys", 0o100644, b"tracked")]);
    run_checkout(&repo, Entry::Reset, &first).expect("first reset");
    let outside = repo.outside.join("outside");
    fs::create_dir(&outside).expect("outside directory");
    fs::write(outside.join("authorized_keys"), b"outside").expect("outside file");
    fs::remove_dir_all(repo.root.join("A")).expect("remove tracked directory");
    std::os::unix::fs::symlink(&outside, repo.root.join("A")).expect("parent symlink");
    let second = repo.commit(&[]);
    run_checkout(&repo, Entry::Reset, &second).expect("second reset");
    assert_eq!(
        fs::read(outside.join("authorized_keys")).expect("outside file survives"),
        b"outside"
    );
    assert!(
        fs::symlink_metadata(repo.root.join("A"))
            .expect("parent unchanged")
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn sparse_reset_removal_treats_symlinked_parent_as_absent() {
    let repo = Repo::new();
    let commit = repo.commit(&[(b"A/authorized_keys", 0o100644, b"tracked")]);
    run_checkout(&repo, Entry::Reset, &commit).expect("first reset");
    let outside = repo.outside.join("outside");
    fs::create_dir(&outside).expect("outside directory");
    fs::write(outside.join("authorized_keys"), b"outside").expect("outside file");
    fs::remove_dir_all(repo.root.join("A")).expect("remove tracked directory");
    std::os::unix::fs::symlink(&outside, repo.root.join("A")).expect("parent symlink");
    fs::write(
        repo.git_dir.join("config"),
        b"[core]\n bare = false\n sparseCheckout = true\n sparseCheckoutCone = false\n",
    )
    .expect("sparse config");
    fs::write(repo.git_dir.join("info/sparse-checkout"), b"/included/\n").expect("sparse patterns");
    run_checkout(&repo, Entry::Reset, &commit).expect("sparse reset");
    assert_eq!(
        fs::read(outside.join("authorized_keys")).expect("outside file survives"),
        b"outside"
    );
}

#[cfg(unix)]
#[test]
fn reset_skips_case_colliding_symlink_and_preserves_both_index_entries() {
    let repo = Repo::new();
    fs::write(
        repo.git_dir.join("config"),
        b"[core]\n bare = false\n ignoreCase = true\n",
    )
    .expect("case-insensitive config");
    let commit = repo.commit(&[
        (b"A/authorized_keys", 0o100644, b"tracked"),
        (b"a", 0o120000, b"../outside"),
    ]);
    run_checkout(&repo, Entry::Reset, &commit).expect("reset");
    assert!(
        !repo.root.join("a").is_symlink(),
        "colliding symlink must not be materialized"
    );
    assert_eq!(
        fs::read(repo.root.join("A/authorized_keys")).expect("first path retained"),
        b"tracked"
    );
    let index = sley_index::Index::parse(
        &fs::read(repo.git_dir.join("index")).expect("index"),
        FORMAT,
    )
    .expect("parse index");
    assert_eq!(index.entries.len(), 2);
    assert_eq!(
        index.entries[1].size, 0,
        "colliding entry has no worktree stat"
    );
}

#[cfg(unix)]
#[test]
fn reset_collision_and_removal_on_case_insensitive_filesystem() {
    let repo = Repo::new();
    // Try setting casefold on an EMPTY directory before creating the repository.
    // Native case-insensitive volumes (e.g. macOS) need no chattr.
    let folded = repo.outside.join("casefold");
    fs::create_dir(&folded).expect("empty casefold directory");
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("chattr")
        .arg("+F")
        .arg(&folded)
        .output();
    fs::write(folded.join("probe"), b"probe").expect("probe");
    let insensitive = folded.join("PROBE").exists();
    fs::remove_file(folded.join("probe")).expect("remove probe");
    if !insensitive {
        eprintln!(
            "skipping casefold reset regression: no case-insensitive filesystem at {}",
            folded.display()
        );
        return;
    }
    // Create fresh directories so they inherit casefold from the parent.
    for dir in ["objects", "refs/heads", "refs/tags", "hooks", "info"] {
        fs::create_dir_all(folded.join("worktree/.git").join(dir)).expect("casefold git directory");
    }
    for file in ["HEAD", "config"] {
        fs::copy(
            repo.git_dir.join(file),
            folded.join("worktree/.git").join(file),
        )
        .expect("fixture config");
    }
    let repo = Repo {
        root: folded.join("worktree"),
        git_dir: folded.join("worktree/.git"),
        db: FileObjectDatabase::from_git_dir(folded.join("worktree/.git"), FORMAT),
        ..repo
    };
    let outside = repo.outside.join("outside");
    fs::create_dir(&outside).expect("outside directory");
    fs::write(outside.join("authorized_keys"), b"outside").expect("outside file");
    let target = outside.as_os_str().as_encoded_bytes();
    let first = repo.commit(&[
        (b"A/authorized_keys", 0o100644, b"tracked"),
        (b"a", 0o120000, target),
    ]);
    run_checkout(&repo, Entry::Reset, &first).expect("first reset");
    assert!(
        fs::symlink_metadata(repo.root.join("A"))
            .expect("directory")
            .is_dir(),
        "reset must skip colliding symlink"
    );
    let second = repo.commit(&[]);
    run_checkout(&repo, Entry::Reset, &second).expect("second reset");
    assert_eq!(
        fs::read(outside.join("authorized_keys")).expect("outside file survives both resets"),
        b"outside"
    );
    // Also exercise an index/worktree left by the old reset implementation:
    // the index retains A/authorized_keys while its parent aliases a symlink.
    run_checkout(&repo, Entry::Reset, &first).expect("restore first commit");
    fs::remove_dir_all(repo.root.join("A")).expect("remove directory");
    std::os::unix::fs::symlink(&outside, repo.root.join("a")).expect("legacy colliding parent");
    run_checkout(&repo, Entry::Reset, &second).expect("remove legacy colliding parent");
    assert_eq!(
        fs::read(outside.join("authorized_keys")).expect("outside file survives"),
        b"outside"
    );
}

#[cfg(unix)]
#[test]
fn legacy_blob_writer_refuses_symlinks_at_leaf_and_in_parents() {
    for parent_link in [false, true] {
        let repo = Repo::new();
        let outside = repo.outside.join("outside");
        fs::create_dir(&outside).expect("outside directory");
        fs::write(outside.join("file"), b"outside").expect("outside file");
        let path = if parent_link {
            std::os::unix::fs::symlink(&outside, repo.root.join("link")).expect("parent link");
            repo.root.join("link/./file")
        } else {
            std::os::unix::fs::symlink(outside.join("file"), repo.root.join("file"))
                .expect("leaf link");
            repo.root.join("file")
        };
        assert!(
            sley_worktree::write_blob_body_or_symlink(&path, 0o100644, b"new", b"new").is_err()
        );
        assert_eq!(
            fs::read(outside.join("file")).expect("outside file survives"),
            b"outside"
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_checkout_refuses_root_backslash_and_win32_names() {
    let policy = sley_worktree::WorktreePathPolicy::default();
    for path in [
        r"\Users\Public\x",
        r"a\b",
        "C:relative",
        "AUX.txt",
        "dir/LPT0",
        "dir/COM9",
        "CONIN$",
        "CONOUT$",
        "a:stream",
        "a.",
        "a ",
        "a?b",
        "a\u{1f}b",
    ] {
        assert!(!policy.is_valid_path(path.as_bytes(), 0o100644), "{path:?}");
        let repo = Repo::new();
        let commit = repo.commit(&[(path.as_bytes(), 0o100644, b"content")]);
        assert!(
            run_checkout(&repo, Entry::Reset, &commit).is_err(),
            "{path:?}"
        );
        assert!(repo.worktree_listing().is_empty());
    }
}

/// Seeded generation makes failures reproducible, with many names unrelated
/// to the fixed regressions, and biased mutations near the platform aliases.
fn randomized_names(count: usize) -> Vec<(Vec<u8>, u32)> {
    let mut state = 0x2492_4755_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let seeds = [
        ".git",
        ".gitmodules",
        "GIT~1",
        "gitmod~4",
        "gi7eba~1",
        ".g\u{200c}it",
        ".git\u{feff}modules",
        "AUX",
        "CONIN$",
        "LPT0",
        "COM9",
        ".github",
        "ordinary",
        "..",
        ".",
        "",
        "a\\b",
        "\\.git",
        "C:foo",
    ];
    let suffixes = [
        "", ".", " ", ":stream", "x", ".. ", "\\hooks", "\u{206a}", "\u{00ad}",
    ];
    let alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._ ~:\\?*<>|";
    let mut names = std::collections::BTreeSet::new();
    while names.len() < count {
        let mut path = String::new();
        for component in 0..(1 + next() % 4) {
            if component != 0 {
                path.push('/');
            }
            if next() % 3 == 0 {
                for _ in 0..(1 + next() % 20) {
                    path.push(char::from(alphabet[next() as usize % alphabet.len()]));
                }
            } else {
                path.push_str(seeds[next() as usize % seeds.len()]);
                path.push_str(suffixes[next() as usize % suffixes.len()]);
            }
        }
        let mode = if next() & 1 == 0 { 0o100644 } else { 0o120000 };
        names.insert((path.into_bytes(), mode));
    }
    names.into_iter().collect()
}

#[test]
fn caller_policy_reaches_parallel_and_sparse_writers() {
    for entry in ENTRY_POINTS {
        for sparse in [false, true] {
            let repo = Repo::new();
            let mut config =
                b"[core]\n bare = false\n[checkout]\n workers = 2\n thresholdForParallelism = 0\n"
                    .to_vec();
            if sparse {
                config.extend_from_slice(
                    b"[core]\n sparseCheckout = true\n sparseCheckoutCone = false\n",
                );
                fs::write(repo.git_dir.join("info/sparse-checkout"), b"/*\n")
                    .expect("sparse patterns");
            }
            fs::write(repo.git_dir.join("config"), config).expect("config");
            let commit = repo.commit(&[
                (b"GIT~1/file", 0o100644, b"ntfs"),
                (".g\u{200c}it/file".as_bytes(), 0o100644, b"hfs"),
            ]);
            let policy = sley_worktree::WorktreePathPolicy::default()
                .protect_ntfs(false)
                .protect_hfs(false);
            run_checkout_with_policy(&repo, entry, &commit, &policy)
                .expect("explicit policy overrides config even in workers");
            assert_eq!(
                fs::read(repo.root.join("GIT~1/file")).expect("ntfs alias"),
                b"ntfs"
            );
        }
    }
}

#[test]
fn missing_git_under_ci_fails_the_differential_test() {
    let scratch = tempfile::tempdir().expect("empty PATH directory");
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "verdicts_match_real_git", "--nocapture"])
        .env("CI", "1")
        .env("PATH", scratch.path())
        .current_dir(scratch.path())
        .output()
        .expect("run differential without git");
    assert!(
        !output.status.success(),
        "differential must fail under CI without git"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("git is required"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn legacy_blob_writer_keeps_regular_file_overwrite_behavior() {
    let root = tempfile::tempdir().expect("worktree");
    let path = root.path().join("file");
    fs::write(&path, b"old and longer").expect("regular file");
    sley_worktree::write_blob_body_or_symlink(&path, 0o100644, b"new", b"new").expect("overwrite");
    assert_eq!(fs::read(path).expect("regular file"), b"new");
}

#[test]
fn sparse_parallel_checkout_runs_collision_pass() {
    let repo = Repo::new();
    // This forces collision detection on case-sensitive runners as well.
    // Two surviving entries keep the worker batching path active after filtering.
    fs::write(
        repo.git_dir.join("config"),
        b"[core]\n bare = false\n ignorecase = true\n[checkout]\n workers = 2\n thresholdForParallelism = 0\n",
    ).expect("parallel case-insensitive config");
    let config = GitConfig::read(repo.git_dir.join("config")).expect("read config");
    assert_eq!(
        sley_worktree::ParallelCheckoutPlan::from_config(&config, 2).worker_count,
        2
    );
    let commit = repo.commit(&[
        (b"A/file", 0o100644, b"first"),
        (b"a/FILE", 0o100644, b"second"),
        (b"b/file", 0o100644, b"worker"),
        (b"excluded/file", 0o100644, b"skip"),
    ]);
    let sparse = sley_worktree::SparseCheckout {
        patterns: vec![b"/A/".to_vec(), b"/a/".to_vec(), b"/b/".to_vec()],
        sparse_index: false,
    };
    sley_worktree::checkout_commit_to_index_and_worktree_sparse(
        None,
        &repo.root,
        &repo.git_dir,
        FORMAT,
        &commit,
        Some((&sparse, sley_worktree::SparseCheckoutMode::Full)),
        Some(&config),
        None,
    )
    .expect("sparse parallel checkout");
    assert_eq!(
        fs::read(repo.root.join("A/file")).expect("first entry"),
        b"first"
    );
    assert_eq!(
        fs::read(repo.root.join("b/file")).expect("second worker entry"),
        b"worker"
    );
    let index = sley_index::Index::parse(
        &fs::read(repo.git_dir.join("index")).expect("index"),
        FORMAT,
    )
    .expect("parse index");
    let collided = index
        .entries
        .iter()
        .find(|entry| entry.path.as_bytes() == b"a/FILE")
        .expect("colliding entry retained in index");
    assert_eq!(
        collided.size, 0,
        "collision pass leaves colliding entry without worktree stat"
    );
    assert!(!collided.is_skip_worktree());
    assert!(
        index
            .entries
            .iter()
            .find(|entry| entry.path.as_bytes() == b"excluded/file")
            .expect("excluded entry")
            .is_skip_worktree()
    );
}

#[test]
fn legacy_blob_writer_accepts_absolute_dot_components() {
    #[cfg(unix)]
    let root = tempfile::tempdir_in("/tmp").expect("absolute-path fixture");
    #[cfg(not(unix))]
    let root = tempfile::tempdir().expect("absolute-path fixture");
    // Exercise /tmp/./<unique directory>/./file without sharing a fixed leaf.
    let path = root
        .path()
        .parent()
        .expect("fixture parent")
        .join(".")
        .join(root.path().file_name().expect("fixture name"))
        .join("./file");
    sley_worktree::write_blob_body_or_symlink(&path, 0o100644, b"new", b"new")
        .expect("absolute OS path with lexical dot");
    assert_eq!(fs::read(root.path().join("file")).expect("file"), b"new");
    let parent_path = root.path().join("../must-not-write");
    assert!(
        sley_worktree::write_blob_body_or_symlink(&parent_path, 0o100644, b"new", b"new").is_err()
    );
}

#[test]
fn legacy_blob_writer_accepts_relative_dot_components() {
    if std::env::var_os("SLEY_DOT_PATH_CHILD").is_some() {
        sley_worktree::write_blob_body_or_symlink(Path::new("./file"), 0o100644, b"new", b"new")
            .expect("relative OS path with lexical dot");
        assert!(
            sley_worktree::write_blob_body_or_symlink(Path::new("../x"), 0o100644, b"new", b"new")
                .is_err()
        );
        return;
    }
    // Isolate cwd without changing process-global state during parallel tests.
    let root = tempfile::tempdir().expect("worktree");
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "legacy_blob_writer_accepts_relative_dot_components",
            "--nocapture",
        ])
        .env("SLEY_DOT_PATH_CHILD", "1")
        .current_dir(root.path())
        .output()
        .expect("relative writer test");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(root.path().join("file")).expect("file"), b"new");
}
