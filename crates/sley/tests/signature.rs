//! Interop tests for the typed [`sley::Signature`] parse-view against the system
//! `git` binary.
//!
//! These build commits and an annotated tag with real `git`, then read them back
//! through [`sley::Repository`] and assert that the typed `name`/`email`/`time`
//! match what git stored *and* that the parse-view re-serializes byte-identically
//! to git's stored ident bytes (`git cat-file -p`). The cases deliberately cover
//! a non-UTC `+HHMM` offset and git's special `-0000` "timezone unknown"
//! sentinel, which must round-trip distinctly from `+0000`.
//!
//! git itself normalizes a human-supplied `-0000` date to `+0000`, so a literal
//! `-0000` ident only arises from *imported* objects (fast-import, clones, CVS/
//! GitHub history) — exactly the case the parse-view exists to serve. The test
//! reproduces that by writing a raw commit object with `git hash-object -t
//! commit`, which stores the bytes verbatim (confirmed below with `git fsck`).
//!
//! The whole file is gated on `git --version` succeeding, so it is a no-op where
//! git is unavailable.

use std::path::Path;
use std::process::Command;

use sley::{ObjectId, Repository, Signature};

/// Run `git args...` in `cwd`, returning trimmed stdout, or `None` if git is
/// missing or the command fails.
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "A U Thor")
        .env("GIT_COMMITTER_EMAIL", "author@example.com")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim_end().to_string())
}

/// Run `git args...` with explicit author/committer dates set, returning trimmed
/// stdout, or `None` on failure.
fn run_git_dated(
    cwd: &Path,
    args: &[&str],
    author_date: &str,
    committer_date: &str,
) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "A U Thor")
        .env("GIT_COMMITTER_EMAIL", "author@example.com")
        .env("GIT_AUTHOR_DATE", author_date)
        .env("GIT_COMMITTER_DATE", committer_date)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim_end().to_string())
}

/// Hash `content` into the object database as an object of `kind`, storing the
/// bytes verbatim, and return the resulting oid. Used to plant a commit whose
/// ident line contains a literal `-0000` that git would otherwise normalize.
fn git_hash_object(cwd: &Path, kind: &str, content: &[u8]) -> Option<String> {
    use std::io::Write;
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "-t", kind, "--stdin"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(content).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim_end().to_string())
}

/// The exact `author `/`committer `/`tagger ` ident bytes git stores for `oid`,
/// read from `git cat-file -p` (the authoritative on-disk form).
fn git_ident_line(cwd: &Path, oid: &str, header: &str) -> Vec<u8> {
    let pretty = run_git(cwd, &["cat-file", "-p", oid]).expect("cat-file -p");
    let prefix = format!("{header} ");
    let line = pretty
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no {header} line in object {oid}"));
    line.as_bytes().to_vec()
}

fn temp_repo(name: &str) -> std::path::PathBuf {
    let tmp = std::env::temp_dir().join(format!(
        "sley-sig-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp repo dir");
    tmp
}

/// Assert that `sig` matches the expected typed fields and re-serializes exactly
/// to `git_bytes` (git's stored ident line).
fn assert_signature_matches_git(
    sig: &Signature,
    git_bytes: &[u8],
    expected_email: &[u8],
    expected_seconds: i64,
    expected_offset_minutes: i16,
    expected_negative_utc: bool,
) {
    assert_eq!(sig.name.as_bytes(), b"A U Thor", "parsed name");
    assert_eq!(sig.email.as_bytes(), expected_email, "parsed email");
    assert_eq!(sig.time.seconds, expected_seconds, "parsed seconds");
    assert_eq!(
        sig.time.timezone_offset_minutes, expected_offset_minutes,
        "parsed tz offset minutes"
    );
    assert_eq!(
        sig.time.negative_utc, expected_negative_utc,
        "parsed -0000 sentinel"
    );
    // The load-bearing guarantee: the parse-view reproduces git's bytes exactly.
    assert_eq!(
        sig.to_ident_bytes(),
        git_bytes,
        "re-serialized ident must equal git's stored bytes:\n  ours: {:?}\n  git:  {:?}",
        String::from_utf8_lossy(&sig.to_ident_bytes()),
        String::from_utf8_lossy(git_bytes),
    );
}

#[test]
fn signature_parse_view_matches_system_git_for_offset_and_negative_zero() {
    let repo_dir = temp_repo("commit");
    if run_git(&repo_dir, &["--version"]).is_none() {
        let _ = std::fs::remove_dir_all(&repo_dir);
        return;
    }
    assert!(run_git(&repo_dir, &["init", "-q", "-b", "main"]).is_some());

    // 1) A commit made the ordinary way with a non-UTC +HHMM offset. git keeps
    //    the offset verbatim, so author/committer both read +0530.
    let normal_oid = run_git_dated(
        &repo_dir,
        &["commit", "-q", "--allow-empty", "-m", "offset commit"],
        "@1700000000 +0530",
        "@1700000000 +0530",
    )
    .and_then(|_| run_git(&repo_dir, &["rev-parse", "HEAD"]))
    .expect("create offset commit");

    // 2) A commit with a literal -0000 committer. git normalizes a human -0000
    //    date to +0000, so we plant the raw object byte-for-byte the way an
    //    imported commit would look, with a +0530 author and a -0000 committer.
    let tree = run_git(&repo_dir, &["write-tree"]).expect("write-tree");
    let raw_commit = format!(
        "tree {tree}\n\
         parent {normal_oid}\n\
         author A U Thor <author@example.com> 1700000000 +0530\n\
         committer A U Thor <author@example.com> 1700000000 -0000\n\
         \n\
         imported commit\n"
    );
    let imported_oid =
        git_hash_object(&repo_dir, "commit", raw_commit.as_bytes()).expect("hash raw commit");
    // git must accept the planted object as valid.
    assert!(
        run_git(&repo_dir, &["fsck", "--strict"]).is_some(),
        "git fsck rejected the planted -0000 commit"
    );

    let repo = Repository::discover(&repo_dir).expect("discover repo");

    // --- Normal +0530 commit: author and committer parse-views match git. ---
    let normal = ObjectId::from_hex(repo.object_format(), &normal_oid).expect("parse oid");
    let commit = repo.read_commit(&normal).expect("read offset commit");

    let git_author = git_ident_line(&repo_dir, &normal_oid, "author");
    let git_committer = git_ident_line(&repo_dir, &normal_oid, "committer");

    let author = commit.author_signature().expect("author parses");
    assert_signature_matches_git(
        &author,
        &git_author,
        b"author@example.com",
        1_700_000_000,
        330,
        false,
    );
    let committer = commit.committer_signature().expect("committer parses");
    assert_signature_matches_git(
        &committer,
        &git_committer,
        b"author@example.com",
        1_700_000_000,
        330,
        false,
    );

    // The repo-level convenience accessor returns the same thing.
    assert_eq!(
        repo.read_commit_author(&normal).expect("read author"),
        Some(author)
    );

    // --- Imported -0000 commit: the sentinel survives and is distinct. ---
    let imported = ObjectId::from_hex(repo.object_format(), &imported_oid).expect("parse oid");
    let imported_commit = repo.read_commit(&imported).expect("read imported commit");

    let git_imported_author = git_ident_line(&repo_dir, &imported_oid, "author");
    let git_imported_committer = git_ident_line(&repo_dir, &imported_oid, "committer");

    let imported_author = imported_commit.author_signature().expect("author parses");
    assert_signature_matches_git(
        &imported_author,
        &git_imported_author,
        b"author@example.com",
        1_700_000_000,
        330,
        false,
    );
    let imported_committer = imported_commit
        .committer_signature()
        .expect("committer parses");
    assert_signature_matches_git(
        &imported_committer,
        &git_imported_committer,
        b"author@example.com",
        1_700_000_000,
        0,
        true,
    );

    // git's stored bytes themselves differ between the +0000 author... wait, the
    // author is +0530 here; assert the -0000 committer bytes literally end in
    // "-0000" and differ from a +0000 rendering, proving the distinction is real.
    assert!(git_imported_committer.ends_with(b" -0000"));
    assert_eq!(imported_committer.time.offset_token(), "-0000");
    assert_ne!(
        imported_committer.to_ident_bytes(),
        // What a naive +0000 collapse would have produced.
        b"A U Thor <author@example.com> 1700000000 +0000".to_vec(),
    );

    let _ = std::fs::remove_dir_all(&repo_dir);
}

#[test]
fn tag_signature_parse_view_matches_system_git() {
    let repo_dir = temp_repo("tag");
    if run_git(&repo_dir, &["--version"]).is_none() {
        let _ = std::fs::remove_dir_all(&repo_dir);
        return;
    }
    assert!(run_git(&repo_dir, &["init", "-q", "-b", "main"]).is_some());
    run_git_dated(
        &repo_dir,
        &["commit", "-q", "--allow-empty", "-m", "base"],
        "@1700000000 +0000",
        "@1700000000 +0000",
    )
    .expect("base commit");

    // An annotated tag with a +0530 tagger offset, made the ordinary way.
    run_git_dated(
        &repo_dir,
        &["tag", "-a", "v1", "-m", "release"],
        "@1700000500 +0530",
        "@1700000500 +0530",
    )
    .expect("create annotated tag");
    let tag_oid = run_git(&repo_dir, &["rev-parse", "v1"]).expect("rev-parse tag");

    let repo = Repository::discover(&repo_dir).expect("discover repo");
    let oid = ObjectId::from_hex(repo.object_format(), &tag_oid).expect("parse oid");
    let tag = repo.read_tag(&oid).expect("read tag");

    let git_tagger = git_ident_line(&repo_dir, &tag_oid, "tagger");
    let tagger = tag.tagger_signature().expect("tagger parses");
    assert_signature_matches_git(
        &tagger,
        &git_tagger,
        b"author@example.com",
        1_700_000_500,
        330,
        false,
    );

    // Repo-level convenience accessor agrees.
    assert_eq!(
        repo.read_tag_tagger(&oid).expect("read tagger"),
        Some(tagger)
    );

    let _ = std::fs::remove_dir_all(&repo_dir);
}
