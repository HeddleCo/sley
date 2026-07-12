//! Differential interop tests for `git grep` vs the system `git` binary.
//!
//! Each case runs the same arguments through both `git` and the `sley` binary
//! in an identical repository and asserts the stdout and exit status match. The
//! whole suite is skipped when `git` is unavailable.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()));
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
    run_env(sley_testkit::oracle_git(), cwd, args)
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed in {}:\n{}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sley(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::sley_bin!(), cwd, args)
}

fn sley_with_trace(cwd: &Path, args: &[&str], trace: &Path) -> Output {
    Command::new(sley_testkit::sley_bin!())
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", "@1790000000 -0500")
        .env("GIT_COMMITTER_DATE", "@1790000000 -0500")
        .env("GIT_TRACE2_EVENT", trace)
        .output()
        .unwrap_or_else(|err| panic!("failed to run sley {args:?}: {err}"))
}

fn assert_trace2_data(trace: &Path, category: &str, key: &str, value: usize) {
    let contents = fs::read_to_string(trace).expect("read trace2 event file");
    let expected = format!("\"category\":\"{category}\",\"key\":\"{key}\",\"value\":\"{value}\"");
    assert!(
        contents.lines().any(|line| line.contains(&expected)),
        "missing trace2 data {category}/{key}={value}:\n{contents}"
    );
}

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Asserts sley and git agree on stdout and exit code for `args` run in `cwd`.
fn assert_same(cwd: &Path, args: &[&str]) {
    let g = git(cwd, args);
    let r = sley(cwd, args);
    assert_eq!(
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
        "stdout differs for grep {args:?}\nsley stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit differs for grep {args:?}"
    );
}

/// Like [`assert_same`] but compares raw stdout bytes (for `-z` / NUL output).
fn assert_same_bytes(cwd: &Path, args: &[&str]) {
    let g = git(cwd, args);
    let r = sley(cwd, args);
    assert_eq!(
        r.stdout,
        g.stdout,
        "stdout bytes differ for grep {args:?}\nsley stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit differs for grep {args:?}"
    );
}

/// Builds a small repository with a couple of tracked text files, a nested
/// directory, and a binary blob, then commits it.
fn build_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    git_ok(root, &["init", "-q", repo.to_str().unwrap_or(".")]);
    fs::write(repo.join("a.txt"), "hello world\nfoo bar\nHELLO again\n")
        .expect("test operation should succeed");
    fs::write(repo.join("b.txt"), "nothing here\nbaz\n").expect("test operation should succeed");
    fs::create_dir_all(repo.join("sub")).expect("test operation should succeed");
    fs::write(repo.join("sub/c.txt"), "hello sub\nworld\n").expect("test operation should succeed");
    fs::write(repo.join("re.txt"), "aaa\na+a\nfoo|bar\nfoobar\n")
        .expect("test operation should succeed");
    fs::write(repo.join("nums.txt"), "123\n456789\nx12y\n").expect("test operation should succeed");
    fs::write(repo.join("bin.dat"), b"foo\x00bar match\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "init"]);
    repo
}

#[test]
fn grep_working_tree_basic_flags_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("grep-basic");
    let repo = build_repo(&root);

    // Default search, line numbers, case-insensitive, list, count, invert.
    assert_same(&repo, &["grep", "hello"]);
    assert_same(&repo, &["grep", "-n", "hello"]);
    assert_same(&repo, &["grep", "-i", "hello"]);
    assert_same(&repo, &["grep", "-l", "hello"]);
    assert_same(&repo, &["grep", "--files-with-matches", "hello"]);
    assert_same(&repo, &["grep", "-c", "hello"]);
    assert_same(&repo, &["grep", "--count", "hello"]);
    assert_same(&repo, &["grep", "-v", "hello", "--", "a.txt"]);
    assert_same(&repo, &["grep", "-c", "-v", "hello", "--", "a.txt"]);
    // Combined short flags.
    assert_same(&repo, &["grep", "-in", "hello"]);
    assert_same(&repo, &["grep", "-ni", "HELLO"]);
    // No filename / forced filename.
    assert_same(&repo, &["grep", "-h", "hello"]);
    assert_same(&repo, &["grep", "-H", "hello", "--", "a.txt"]);
    // A pattern that does not match anything (exit code 1).
    assert_same(&repo, &["grep", "definitely-not-present"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn grep_word_and_fixed_string_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("grep-word-fixed");
    let repo = build_repo(&root);

    assert_same(&repo, &["grep", "-w", "bar"]);
    assert_same(&repo, &["grep", "-w", "ba"]);
    assert_same(&repo, &["grep", "-w", "-i", "hello"]);
    // Fixed strings treat regex metacharacters literally.
    assert_same(&repo, &["grep", "-F", "foo bar"]);
    assert_same(&repo, &["grep", "-F", "foo|bar"]);
    assert_same(&repo, &["grep", "--fixed-strings", "a+a"]);
    // Multiple patterns via -e are OR-combined.
    assert_same(&repo, &["grep", "-e", "hello", "-e", "baz"]);
    assert_same(&repo, &["grep", "-n", "-e", "hello", "-e", "baz"]);
    assert_same(
        &repo,
        &["grep", "--all-match", "-e", "hello", "-e", "world"],
    );
    assert_same(
        &repo,
        &["grep", "-L", "--all-match", "-e", "hello", "-e", "baz"],
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn grep_basic_and_extended_regex_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("grep-regex");
    let repo = build_repo(&root);

    // Basic regular expressions (Git's default): `+` is literal, `\+` repeats.
    assert_same(&repo, &["grep", "a+", "--", "re.txt"]);
    assert_same(&repo, &["grep", r"a\+", "--", "re.txt"]);
    assert_same(&repo, &["grep", "foo|bar", "--", "re.txt"]);
    assert_same(&repo, &["grep", "h.llo", "--", "a.txt"]);
    assert_same(&repo, &["grep", "^hello", "--", "a.txt"]);
    assert_same(&repo, &["grep", "world$", "--", "a.txt"]);
    assert_same(&repo, &["grep", r"[0-9]\{3\}", "--", "nums.txt"]);
    assert_same(&repo, &["grep", "[[:digit:]]", "--", "nums.txt"]);

    // Extended regular expressions.
    assert_same(&repo, &["grep", "-E", "a+", "--", "re.txt"]);
    assert_same(
        &repo,
        &["grep", "--extended-regexp", "foo|bar", "--", "re.txt"],
    );
    assert_same(&repo, &["grep", "-E", "a{2,3}", "--", "re.txt"]);
    assert_same(&repo, &["grep", "-E", "[0-9]{3}", "--", "nums.txt"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn grep_cached_and_revision_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("grep-rev");
    let repo = build_repo(&root);

    // --cached searches the index; tree-ishes prefix output with the rev.
    assert_same(&repo, &["grep", "--cached", "hello"]);
    assert_same(&repo, &["grep", "hello", "HEAD"]);
    assert_same(&repo, &["grep", "-n", "hello", "HEAD"]);
    assert_same(&repo, &["grep", "-l", "hello", "HEAD"]);
    assert_same(&repo, &["grep", "-c", "hello", "HEAD"]);
    // A rev followed by a path filter (no `--`).
    assert_same(&repo, &["grep", "hello", "HEAD", "a.txt"]);
    // Working-tree edit is visible by default but not via --cached.
    fs::write(
        repo.join("a.txt"),
        "hello world\nfoo bar\nHELLO again\nWTONLY zz\n",
    )
    .expect("test operation should succeed");
    assert_same(&repo, &["grep", "WTONLY"]);
    assert_same(&repo, &["grep", "--cached", "WTONLY"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn grep_pathspec_limiting_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("grep-pathspec");
    let repo = build_repo(&root);

    assert_same(&repo, &["grep", "hello", "--", "a.txt"]);
    assert_same(&repo, &["grep", "hello", "--", "sub"]);
    assert_same(&repo, &["grep", "-l", "hello", "--", "*.txt"]);
    assert_same(&repo, &["grep", "hello", "a.txt", "b.txt"]);
    assert_same(&repo, &["grep", "-l", "hello", "--", "sub/*"]);

    // From a subdirectory: search is scoped to the cwd and paths are relative
    // to it, unless --full-name is given.
    let sub = repo.join("sub");
    assert_same(&sub, &["grep", "hello"]);
    assert_same(&sub, &["grep", "-n", "world"]);
    assert_same(&sub, &["grep", "-l", "hello"]);
    assert_same(&sub, &["grep", "--full-name", "-l", "hello"]);
    assert_same(&sub, &["grep", "hello", "HEAD"]);
    assert_same(&sub, &["grep", "--full-name", "hello", "HEAD"]);
    fs::write(sub.join("file2"), "world\n").expect("test operation should succeed");
    assert_same(&sub, &["grep", "--untracked", "o"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn grep_binary_and_null_output_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("grep-binary-z");
    let repo = build_repo(&root);

    // Binary files report a summary line by default, content with -a, nothing
    // with -I, but still count / list normally.
    assert_same(&repo, &["grep", "match", "--", "bin.dat"]);
    assert_same(&repo, &["grep", "-a", "match", "--", "bin.dat"]);
    assert_same(&repo, &["grep", "-I", "match", "--", "bin.dat"]);
    assert_same(&repo, &["grep", "-c", "match", "--", "bin.dat"]);
    assert_same(&repo, &["grep", "-l", "match", "--", "bin.dat"]);

    // NUL-separated output (`-z`) uses NUL field separators with newline line
    // terminators, and NUL-terminated path lists.
    assert_same_bytes(&repo, &["grep", "-z", "hello"]);
    assert_same_bytes(&repo, &["grep", "-z", "-n", "hello"]);
    assert_same_bytes(&repo, &["grep", "-z", "-l", "hello"]);
    assert_same_bytes(&repo, &["grep", "-z", "-c", "hello"]);
    assert_same_bytes(&repo, &["grep", "-z", "-n", "hello", "HEAD"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn grep_only_matching_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("grep-only");
    let repo = build_repo(&root);
    fs::write(
        repo.join("mmap.txt"),
        "foo mmap bar\nfoo_mmap bar mmap\nfoo mmap bar_mmap\n",
    )
    .expect("test operation should succeed");
    git_ok(&repo, &["add", "mmap.txt"]);
    git_ok(&repo, &["commit", "-qm", "mmap"]);

    assert_same(&repo, &["grep", "-o", "hello", "--", "a.txt"]);
    assert_same(&repo, &["grep", "-o", "-n", "hello", "--", "a.txt"]);
    assert_same(
        &repo,
        &["grep", "--column", "-n", "-o", "mmap", "--", "mmap.txt"],
    );
    assert_same(
        &repo,
        &[
            "grep", "--column", "-n", "-o", "mmap", "HEAD", "--", "mmap.txt",
        ],
    );
    assert_same(&repo, &["grep", "-o", r"[0-9]\{3\}", "--", "nums.txt"]);
    assert_same(&repo, &["grep", "-o", "-E", "[0-9]+", "--", "nums.txt"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn grep_historical_partial_submodule_lazy_fetch_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("grep-partial-submodule");
    let source = root.join("source");
    let submodule = source.join("sub");
    let expected = root.join("expected");
    let actual = root.join("actual");
    git_ok(&root, &["init", "-q", source.to_str().unwrap_or("source")]);
    fs::write(source.join("super-file"), "Some content for super-file\n")
        .expect("write superproject file");
    git_ok(&source, &["add", "super-file"]);
    git_ok(&source, &["commit", "-qm", "superproject"]);

    git_ok(
        &source,
        &["init", "-q", submodule.to_str().unwrap_or("sub")],
    );
    fs::write(submodule.join("sub-file"), "Some content for sub-file\n")
        .expect("write submodule file");
    git_ok(&submodule, &["add", "sub-file"]);
    git_ok(&submodule, &["commit", "-qm", "submodule"]);
    git_ok(
        &source,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "./sub",
        ],
    );
    git_ok(&source, &["commit", "-qm", "add submodule"]);
    fs::write(
        submodule.join("sub-file"),
        "Some content for sub-file\nSome more content for sub-file\n",
    )
    .expect("update submodule file");
    git_ok(&submodule, &["add", "sub-file"]);
    git_ok(&submodule, &["commit", "-qm", "update submodule"]);
    git_ok(&source, &["add", "sub"]);
    git_ok(&source, &["commit", "-qm", "update gitlink"]);
    for repo in [&source, &submodule] {
        git_ok(repo, &["config", "uploadpack.allowFilter", "true"]);
        git_ok(repo, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    }

    let source_url = format!("file://{}", source.display());
    let clone_args = |destination: &Path| {
        vec![
            "-c".to_string(),
            "protocol.file.allow=always".to_string(),
            "clone".to_string(),
            "-q".to_string(),
            "--filter=blob:none".to_string(),
            "--also-filter-submodules".to_string(),
            "--recurse-submodules".to_string(),
            source_url.clone(),
            destination.to_string_lossy().into_owned(),
        ]
    };
    let expected_args = clone_args(&expected);
    let expected_refs = expected_args.iter().map(String::as_str).collect::<Vec<_>>();
    assert!(
        git(&root, &expected_refs).status.success(),
        "oracle filtered recursive clone should succeed"
    );
    let actual_args = clone_args(&actual);
    let actual_refs = actual_args.iter().map(String::as_str).collect::<Vec<_>>();
    let cloned = sley(&root, &actual_refs);
    assert!(
        cloned.status.success(),
        "sley filtered recursive clone failed: {}",
        String::from_utf8_lossy(&cloned.stderr)
    );

    let args = ["grep", "-e", "content", "--recurse-submodules", "HEAD^"];
    let expected_grep = git(&expected, &args);
    let actual_grep = sley(&actual, &args);
    assert_eq!(actual_grep.status.code(), expected_grep.status.code());
    assert_eq!(actual_grep.stdout, expected_grep.stdout);
    assert_eq!(
        String::from_utf8_lossy(&actual_grep.stdout),
        "HEAD^:sub/sub-file:Some content for sub-file\nHEAD^:super-file:Some content for super-file\n"
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn grep_revision_batches_pathspec_selected_promisor_blobs() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("grep-promisor-batch");
    let source = root.join("source");
    let clone = root.join("clone");
    git_ok(&root, &["init", "-q", source.to_str().unwrap_or("source")]);
    fs::create_dir_all(source.join("a")).expect("create source a");
    fs::create_dir_all(source.join("b")).expect("create source b");
    fs::write(source.join("a/matches.txt"), "needle in haystack\n").expect("write match");
    fs::write(source.join("a/nomatch.txt"), "nothing to see here\n").expect("write nonmatch");
    fs::write(source.join("b/matches.md"), "needle again\n").expect("write second match");
    git_ok(&source, &["add", "."]);
    git_ok(&source, &["commit", "-qm", "initial"]);
    git_ok(&source, &["config", "uploadpack.allowFilter", "true"]);
    git_ok(
        &source,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    );

    let source_url = format!("file://{}", source.display());
    let cloned = sley(
        &root,
        &[
            "clone",
            "-q",
            "--no-checkout",
            "--filter=blob:none",
            &source_url,
            clone.to_str().unwrap_or("clone"),
        ],
    );
    assert!(
        cloned.status.success(),
        "sley filtered clone failed: {}",
        String::from_utf8_lossy(&cloned.stderr)
    );

    let pathspec_trace = root.join("pathspec.trace");
    let pathspec = sley_with_trace(
        &clone,
        &["grep", "-c", "needle", "HEAD", "--", "a/*.txt"],
        &pathspec_trace,
    );
    assert!(pathspec.status.success());
    assert_eq!(
        String::from_utf8_lossy(&pathspec.stdout),
        "HEAD:a/matches.txt:1\n"
    );
    assert_trace2_data(&pathspec_trace, "promisor", "fetch_count", 2);
    assert_trace2_data(&pathspec_trace, "pack-objects", "written", 2);

    let all_trace = root.join("all.trace");
    let all = sley_with_trace(&clone, &["grep", "-c", "needle", "HEAD"], &all_trace);
    assert!(all.status.success());
    assert_eq!(
        String::from_utf8_lossy(&all.stdout),
        "HEAD:a/matches.txt:1\nHEAD:b/matches.md:1\n"
    );
    assert_trace2_data(&all_trace, "promisor", "fetch_count", 1);
    assert_trace2_data(&all_trace, "pack-objects", "written", 1);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn grep_recursive_submodule_replace_config_is_repository_scoped() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("grep-submodule-replacements");
    let repo = root.join("base");
    let submodule = repo.join("sub");
    git_ok(&root, &["init", "-q", repo.to_str().unwrap_or("base")]);
    git_ok(&repo, &["init", "-q", submodule.to_str().unwrap_or("sub")]);
    fs::write(repo.join("a"), "A\n").expect("write a");
    fs::write(repo.join("b"), "B\n").expect("write b");
    fs::write(submodule.join("c"), "C\n").expect("write c");
    fs::write(submodule.join("d"), "D\n").expect("write d");
    git_ok(&submodule, &["add", "c", "d"]);
    git_ok(&submodule, &["commit", "-qm", "submodule files"]);
    git_ok(
        &repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "./sub",
        ],
    );
    git_ok(&repo, &["add", "a", "b", "sub"]);
    git_ok(&repo, &["commit", "-qm", "superproject files"]);

    let object_id = |cwd: &Path, revision: &str| {
        let output = git(cwd, &["rev-parse", revision]);
        assert!(output.status.success(), "rev-parse {revision} failed");
        String::from_utf8(output.stdout)
            .expect("object id is utf8")
            .trim()
            .to_string()
    };
    let a = object_id(&repo, "HEAD:a");
    let b = object_id(&repo, "HEAD:b");
    let c = object_id(&submodule, "HEAD:c");
    let d = object_id(&submodule, "HEAD:d");
    git_ok(&repo, &["replace", &a, &b]);
    git_ok(&submodule, &["replace", &c, &d]);

    let grep_a = ["grep", "--cached", "--recurse-submodules", "A"];
    let grep_c = ["grep", "--cached", "--recurse-submodules", "C"];
    assert_same(&repo, &grep_a);
    assert_same(&repo, &grep_c);

    git_ok(&repo, &["config", "core.useReplaceRefs", "false"]);
    assert_same(&repo, &grep_a);
    assert_same(&repo, &grep_c);

    git_ok(&submodule, &["config", "core.useReplaceRefs", "false"]);
    assert_same(&repo, &grep_a);
    assert_same(&repo, &grep_c);

    git_ok(&repo, &["config", "--unset", "core.useReplaceRefs"]);
    assert_same(&repo, &grep_a);
    assert_same(&repo, &grep_c);
    fs::remove_dir_all(&root).ok();
}
