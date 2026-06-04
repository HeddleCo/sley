//! Differential interop tests for `git merge-file` vs the system `git`.
//!
//! Each case runs the same invocation through the system `git` and through
//! `git-rs` and asserts identical stdout, stderr (where relevant) and exit
//! status. The fixtures stay on the "agreement set" where git-rs' merge engine
//! (`git_diff_merge::merge_blobs` and its primitives) reproduces upstream git
//! byte-for-byte: clean merges and conflicts whose changed regions are
//! well-separated and do not share context lines with the other side. (git's
//! zealous trimming and sub-marker-size conflict coalescing are deliberately
//! out of scope, matching the engine's documented behaviour.)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("git-rs-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).expect("create temp root");
    path
}

fn run_env(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", "@1790000000 -0500")
        .env("GIT_COMMITTER_DATE", "@1790000000 -0500")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run_env("git", cwd, args)
}

fn git_rs(cwd: &Path, args: &[&str]) -> Output {
    run_env(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Write the three standard fixture files into `dir` and return their names.
fn write_inputs(dir: &Path, cur: &[u8], base: &[u8], other: &[u8]) -> (String, String, String) {
    fs::write(dir.join("cur.txt"), cur).expect("write cur");
    fs::write(dir.join("base.txt"), base).expect("write base");
    fs::write(dir.join("other.txt"), other).expect("write other");
    (
        "cur.txt".to_string(),
        "base.txt".to_string(),
        "other.txt".to_string(),
    )
}

/// Run the same `merge-file` args (with `-p`, so no files are mutated) through
/// both binaries and assert identical stdout and exit code. `dir` already holds
/// the fixture files referenced by `args`.
fn assert_stdout_merge(dir: &Path, args: &[&str]) {
    let g = git(dir, args);
    let r = git_rs(dir, args);
    assert_eq!(
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
        "stdout differs for {args:?}\ngit-rs stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit differs for {args:?}\ngit stderr: {}\ngit-rs stderr: {}",
        String::from_utf8_lossy(&g.stderr),
        String::from_utf8_lossy(&r.stderr)
    );
}

/// Compare an in-place (no `-p`) merge: run each binary in its own copy of the
/// fixtures and assert the rewritten current file and exit code match.
fn assert_inplace_merge(cur: &[u8], base: &[u8], other: &[u8], extra: &[&str]) {
    let root = unique_temp_dir("merge-file-inplace");
    let git_dir = root.join("git");
    let rs_dir = root.join("rs");
    fs::create_dir_all(&git_dir).expect("git dir");
    fs::create_dir_all(&rs_dir).expect("rs dir");
    let (cur_name, base_name, other_name) = write_inputs(&git_dir, cur, base, other);
    write_inputs(&rs_dir, cur, base, other);

    let mut args: Vec<&str> = extra.to_vec();
    args.extend_from_slice(&[cur_name.as_str(), base_name.as_str(), other_name.as_str()]);

    let g = git(&git_dir, &args);
    let r = git_rs(&rs_dir, &args);

    let g_file = fs::read(git_dir.join("cur.txt")).expect("git cur");
    let r_file = fs::read(rs_dir.join("cur.txt")).expect("rs cur");
    assert_eq!(
        String::from_utf8_lossy(&r_file),
        String::from_utf8_lossy(&g_file),
        "in-place result differs for {args:?}\ngit-rs stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "in-place exit differs for {args:?}"
    );
    fs::remove_dir_all(&root).ok();
}

/// A clean three-way merge (disjoint, well-separated edits) is byte-identical
/// and exits 0, both to stdout and in place.
#[test]
fn clean_merge_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-clean");
    let cur = b"OURS\nl2\nl3\nl4\nl5\nl6\nl7\n";
    let base = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\n";
    let other = b"l1\nl2\nl3\nl4\nl5\nl6\nTHEIRS\n";
    let (c, b, o) = write_inputs(&root, cur, base, other);
    assert_stdout_merge(&root, &["merge-file", "-p", &c, &b, &o]);
    assert_inplace_merge(cur, base, other, &[]);
    fs::remove_dir_all(&root).ok();
}

/// A single conflict: markers, default labels (the file paths) and exit status 1
/// must all match, both to stdout and written in place.
#[test]
fn single_conflict_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-conflict");
    let cur = b"l1\nOURS\nl3\n";
    let base = b"l1\nl2\nl3\n";
    let other = b"l1\nTHEIRS\nl3\n";
    let (c, b, o) = write_inputs(&root, cur, base, other);
    assert_stdout_merge(&root, &["merge-file", "-p", &c, &b, &o]);
    assert_inplace_merge(cur, base, other, &[]);
    fs::remove_dir_all(&root).ok();
}

/// Two well-separated conflicts: exit status is the conflict count (2).
#[test]
fn multiple_conflicts_exit_count_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-multi");
    let cur = b"A_OURS\nb\nc\nd\ne\nf\ng\nH_OURS\n";
    let base = b"a\nb\nc\nd\ne\nf\ng\nh\n";
    let other = b"A_THEIRS\nb\nc\nd\ne\nf\ng\nH_THEIRS\n";
    let (c, b, o) = write_inputs(&root, cur, base, other);
    assert_stdout_merge(&root, &["merge-file", "-p", &c, &b, &o]);
    fs::remove_dir_all(&root).ok();
}

/// `-q` does not change stdout/exit for a normal text merge; it only suppresses
/// warnings (none in this git version). Verified to match regardless.
#[test]
fn quiet_flag_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-quiet");
    let cur = b"l1\nOURS\nl3\n";
    let base = b"l1\nl2\nl3\n";
    let other = b"l1\nTHEIRS\nl3\n";
    let (c, b, o) = write_inputs(&root, cur, base, other);
    assert_stdout_merge(&root, &["merge-file", "-p", "-q", &c, &b, &o]);
    fs::remove_dir_all(&root).ok();
}

/// `--diff3` adds the `|||||||` base section.
#[test]
fn diff3_style_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-diff3");
    let cur = b"a\nb\nX\nd\ne\n";
    let base = b"a\nb\nc\nd\ne\n";
    let other = b"a\nb\nY\nd\ne\n";
    let (c, b, o) = write_inputs(&root, cur, base, other);
    assert_stdout_merge(&root, &["merge-file", "-p", "--diff3", &c, &b, &o]);
    fs::remove_dir_all(&root).ok();
}

/// `--zdiff3` hoists shared context out of the conflict and shows the base
/// section. The fixture deliberately shares a trailing line on both sides.
#[test]
fn zdiff3_style_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-zdiff3");
    let cur = b"a\nFOO\nshared\nz\n";
    let base = b"a\nz\n";
    let other = b"a\nBAR\nshared\nz\n";
    let (c, b, o) = write_inputs(&root, cur, base, other);
    assert_stdout_merge(&root, &["merge-file", "-p", "--zdiff3", &c, &b, &o]);
    fs::remove_dir_all(&root).ok();
}

/// `--ours`, `--theirs` and `--union` each resolve the conflict (exit 0) with no
/// markers.
#[test]
fn favor_modes_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-favor");
    let cur = b"l1\nOURS\nl3\n";
    let base = b"l1\nl2\nl3\n";
    let other = b"l1\nTHEIRS\nl3\n";
    let (c, b, o) = write_inputs(&root, cur, base, other);
    assert_stdout_merge(&root, &["merge-file", "-p", "--ours", &c, &b, &o]);
    assert_stdout_merge(&root, &["merge-file", "-p", "--theirs", &c, &b, &o]);
    assert_stdout_merge(&root, &["merge-file", "-p", "--union", &c, &b, &o]);
    fs::remove_dir_all(&root).ok();
}

/// `--union` across two separated conflicts concatenates both sides at each.
#[test]
fn union_multiple_regions_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-union");
    let cur = b"A_OURS\nb\nc\nd\ne\nf\ng\nH_OURS\n";
    let base = b"a\nb\nc\nd\ne\nf\ng\nh\n";
    let other = b"A_THEIRS\nb\nc\nd\ne\nf\ng\nH_THEIRS\n";
    let (c, b, o) = write_inputs(&root, cur, base, other);
    assert_stdout_merge(&root, &["merge-file", "-p", "--union", &c, &b, &o]);
    fs::remove_dir_all(&root).ok();
}

/// `-L` labels: three labels set ours/base/theirs; the base label only shows in
/// diff3. Also exercise the glued `-L<name>` short form.
#[test]
fn custom_labels_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-labels");
    let cur = b"a\nb\nX\nd\ne\n";
    let base = b"a\nb\nc\nd\ne\n";
    let other = b"a\nb\nY\nd\ne\n";
    let (c, b, o) = write_inputs(&root, cur, base, other);
    assert_stdout_merge(
        &root,
        &[
            "merge-file",
            "-p",
            "-L",
            "MINE",
            "-L",
            "ORIG",
            "-L",
            "YOURS",
            &c,
            &b,
            &o,
        ],
    );
    assert_stdout_merge(
        &root,
        &[
            "merge-file",
            "-p",
            "--diff3",
            "-L",
            "MINE",
            "-L",
            "ORIG",
            "-L",
            "YOURS",
            &c,
            &b,
            &o,
        ],
    );
    // Glued short form and the two-label case (second label names the base).
    assert_stdout_merge(
        &root,
        &[
            "merge-file",
            "-p",
            "--diff3",
            "-LMINE",
            "-LBASE",
            &c,
            &b,
            &o,
        ],
    );
    fs::remove_dir_all(&root).ok();
}

/// `--marker-size` changes the marker length.
#[test]
fn marker_size_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-marker");
    let cur = b"l1\nOURS\nl3\n";
    let base = b"l1\nl2\nl3\n";
    let other = b"l1\nTHEIRS\nl3\n";
    let (c, b, o) = write_inputs(&root, cur, base, other);
    assert_stdout_merge(&root, &["merge-file", "-p", "--marker-size=10", &c, &b, &o]);
    assert_stdout_merge(
        &root,
        &["merge-file", "-p", "--marker-size", "4", &c, &b, &o],
    );
    fs::remove_dir_all(&root).ok();
}

/// A conflicting side without a trailing newline at EOF is preserved exactly,
/// with the marker forced onto its own line.
#[test]
fn no_newline_at_eof_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-nonl");
    let cur = b"l1\nOURS"; // no trailing newline
    let base = b"l1\nl2\n";
    let other = b"l1\nTHEIRS\n";
    let (c, b, o) = write_inputs(&root, cur, base, other);
    assert_stdout_merge(&root, &["merge-file", "-p", &c, &b, &o]);
    fs::remove_dir_all(&root).ok();
}

/// A missing input file is the fatal `Could not stat` error: identical stderr
/// and exit status (255) for each of the three positions.
#[test]
fn missing_file_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-missing");
    let (c, b, o) = write_inputs(&root, b"a\n", b"a\n", b"a\n");

    for args in [
        ["merge-file", "-p", "nope.txt", &b, &o],
        ["merge-file", "-p", &c, "nope.txt", &o],
        ["merge-file", "-p", &c, &b, "nope.txt"],
    ] {
        let g = git(&root, &args);
        let r = git_rs(&root, &args);
        assert_eq!(
            String::from_utf8_lossy(&r.stderr),
            String::from_utf8_lossy(&g.stderr),
            "stderr differs for {args:?}"
        );
        assert_eq!(
            r.status.code(),
            g.status.code(),
            "exit differs for {args:?}"
        );
    }
    fs::remove_dir_all(&root).ok();
}

/// Binary input (a NUL byte) is refused with the `Cannot merge binary files`
/// error naming the offending file, exit 255. git checks ours, then base, then
/// theirs.
#[test]
fn binary_input_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-binary");

    // Only the current file is binary.
    let (c, b, o) = write_inputs(&root, b"a\x00b\nOURS\n", b"a\nb\n", b"a\nTHEIRS\n");
    let args = ["merge-file", "-p", &c, &b, &o];
    let g = git(&root, &args);
    let r = git_rs(&root, &args);
    assert_eq!(
        String::from_utf8_lossy(&r.stderr),
        String::from_utf8_lossy(&g.stderr),
        "binary stderr differs (ours binary)"
    );
    assert_eq!(r.status.code(), g.status.code(), "binary exit differs");

    // Only the other file is binary -> git names it.
    write_inputs(&root, b"a\nOURS\n", b"a\nb\n", b"a\x00b\nTHEIRS\n");
    let g = git(&root, &args);
    let r = git_rs(&root, &args);
    assert_eq!(
        String::from_utf8_lossy(&r.stderr),
        String::from_utf8_lossy(&g.stderr),
        "binary stderr differs (theirs binary)"
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "binary exit differs (theirs)"
    );

    fs::remove_dir_all(&root).ok();
}

/// `-q` suppresses the binary diagnostic but keeps the 255 exit status.
#[test]
fn binary_input_quiet_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-binary-q");
    let (c, b, o) = write_inputs(&root, b"a\x00b\nOURS\n", b"a\nb\n", b"a\nTHEIRS\n");
    let args = ["merge-file", "-p", "-q", &c, &b, &o];
    let g = git(&root, &args);
    let r = git_rs(&root, &args);
    assert_eq!(
        String::from_utf8_lossy(&r.stderr),
        String::from_utf8_lossy(&g.stderr),
        "quiet binary stderr differs"
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "quiet binary exit differs"
    );
    fs::remove_dir_all(&root).ok();
}

/// Usage errors: too few / too many operands, too many `-L` labels, an unknown
/// option, and an invalid `--marker-size` all share git's usage text and exit
/// 129.
#[test]
fn usage_errors_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-usage");
    let (c, b, o) = write_inputs(&root, b"a\n", b"a\n", b"a\n");

    let cases: Vec<Vec<&str>> = vec![
        vec!["merge-file"],
        vec!["merge-file", &c, &b],
        vec!["merge-file", &c, &b, &o, "extra.txt"],
        vec![
            "merge-file",
            "-L",
            "1",
            "-L",
            "2",
            "-L",
            "3",
            "-L",
            "4",
            &c,
            &b,
            &o,
        ],
        vec!["merge-file", "--bogus", &c, &b, &o],
        vec!["merge-file", "--marker-size=abc", &c, &b, &o],
        vec!["merge-file", "--diff-algorithm=bogus", &c, &b, &o],
        // Missing option/switch values: diagnostic only, no usage block.
        vec!["merge-file", &c, &b, &o, "--marker-size"],
        vec!["merge-file", &c, &b, &o, "-L"],
    ];

    for args in cases {
        let g = git(&root, &args);
        let r = git_rs(&root, &args);
        assert_eq!(
            String::from_utf8_lossy(&r.stderr),
            String::from_utf8_lossy(&g.stderr),
            "usage stderr differs for {args:?}"
        );
        assert_eq!(
            r.status.code(),
            g.status.code(),
            "usage exit differs for {args:?}"
        );
    }
    fs::remove_dir_all(&root).ok();
}

/// `--object-id` reads blobs by id and, without `-p`, writes the merged blob and
/// prints its id; with `-p` it prints the merged content. Exercised inside a
/// real repository.
#[test]
fn object_id_mode_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-file-oid");
    let repo = root.join("repo");
    let init = git(&root, &["init", "-q", repo.to_str().expect("utf8 path")]);
    assert!(init.status.success(), "git init failed");

    // Hash three blobs into the repo and capture their ids.
    let hash = |bytes: &[u8]| -> String {
        fs::write(repo.join("scratch"), bytes).expect("write scratch");
        let out = git(&repo, &["hash-object", "-w", "scratch"]);
        assert!(out.status.success(), "hash-object failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let ours = hash(b"l1\nOURS\nl3\n");
    let base = hash(b"l1\nl2\nl3\n");
    let theirs = hash(b"l1\nTHEIRS\nl3\n");

    // -p: merged content to stdout, conflict markers labelled with the oids.
    assert_stdout_merge(
        &repo,
        &["merge-file", "--object-id", "-p", &ours, &base, &theirs],
    );

    // Without -p: a new blob is written and its id printed. Both binaries must
    // print the same id (and write the same object) and share the exit status.
    let args = ["merge-file", "--object-id", &ours, &base, &theirs];
    let g = git(&repo, &args);
    let r = git_rs(&repo, &args);
    assert_eq!(
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
        "object-id stdout differs\ngit-rs stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(r.status.code(), g.status.code(), "object-id exit differs");
    fs::remove_dir_all(&root).ok();
}
