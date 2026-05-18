use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("git-rs-{name}-{}-{nanos}", std::process::id()))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_output_with_fixed_identity(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Example User")
        .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
        .env("GIT_AUTHOR_DATE", "@1 +0000")
        .env("GIT_COMMITTER_NAME", "Example User")
        .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
        .env("GIT_COMMITTER_DATE", "@1 +0000")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn assert_same_output(actual: Output, expected: Output, args: &[&str]) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}"
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "stdout differed for {args:?}"
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "stderr differed for {args:?}"
    );
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run_success("git", cwd, args)
}

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run_success(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
}

fn git_stash_push_with_identity(
    cwd: &Path,
    message: &str,
    author_name: &str,
    author_email: &str,
    committer_name: &str,
    committer_email: &str,
) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["stash", "push", "-q", "-m", message])
        .env("GIT_AUTHOR_NAME", author_name)
        .env("GIT_AUTHOR_EMAIL", author_email)
        .env("GIT_COMMITTER_NAME", committer_name)
        .env("GIT_COMMITTER_EMAIL", committer_email)
        .output()
        .expect("run git stash with identity");
    assert!(
        output.status.success(),
        "git stash push failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stash_push_with_dates(cwd: &Path, message: &str, author_date: &str, committer_date: &str) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["stash", "push", "-q", "-m", message])
        .env("GIT_AUTHOR_DATE", author_date)
        .env("GIT_COMMITTER_DATE", committer_date)
        .output()
        .expect("run git stash with dates");
    assert!(
        output.status.success(),
        "git stash push failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn prepare_stash_repo(root: &Path) {
    fs::create_dir_all(root).expect("create temp repo");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
    fs::write(root.join("a.txt"), b"base\n").expect("write base fixture");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base", "-q"]);
    fs::write(root.join("a.txt"), b"one\n").expect("write first stash fixture");
    git(root, &["stash", "push", "-q", "-m", "one"]);
    fs::write(root.join("a.txt"), b"two\n").expect("write second stash fixture");
    git(root, &["stash", "push", "-q", "-m", "two"]);
}

fn prepare_stash_identity_repo(root: &Path) {
    fs::create_dir_all(root).expect("create temp repo");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
    fs::write(root.join("a.txt"), b"base\n").expect("write base fixture");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base", "-q"]);
    fs::write(root.join("a.txt"), b"one\n").expect("write first stash fixture");
    git_stash_push_with_identity(
        root,
        "one",
        "Author One",
        "author1@example.invalid",
        "Committer One",
        "committer1@example.invalid",
    );
    fs::write(root.join("a.txt"), b"two\n").expect("write second stash fixture");
    git_stash_push_with_identity(
        root,
        "two",
        "Author Two",
        "author2@example.invalid",
        "Committer Two",
        "committer2@example.invalid",
    );
}

fn prepare_stash_age_repo(root: &Path) {
    fs::create_dir_all(root).expect("create temp repo");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
    fs::write(root.join("a.txt"), b"base\n").expect("write base fixture");
    git(root, &["add", "a.txt"]);
    let output = Command::new("git")
        .current_dir(root)
        .args(["commit", "-m", "base", "-q"])
        .env("GIT_AUTHOR_DATE", "@100 +0000")
        .env("GIT_COMMITTER_DATE", "@100 +0000")
        .output()
        .expect("run git commit with dates");
    assert!(
        output.status.success(),
        "git commit failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(root.join("a.txt"), b"old\n").expect("write old stash fixture");
    git_stash_push_with_dates(root, "old", "@1000 +0000", "@1000 +0000");
    fs::write(root.join("a.txt"), b"new\n").expect("write new stash fixture");
    git_stash_push_with_dates(root, "new", "@2000 +0000", "@2000 +0000");
}

fn prepare_untracked_stash_repo(root: &Path) {
    fs::create_dir_all(root).expect("create temp repo");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
    fs::write(root.join("z.txt"), b"base\n").expect("write base fixture");
    git(root, &["add", "z.txt"]);
    git(root, &["commit", "-m", "base", "-q"]);
    fs::write(root.join("z.txt"), b"tracked\n").expect("write tracked fixture");
    fs::write(root.join("a.txt"), b"untracked\n").expect("write untracked fixture");
    git(root, &["stash", "push", "-q", "-u", "-m", "with untracked"]);
}

fn prepare_tracked_only_stash_repo(root: &Path) {
    fs::create_dir_all(root).expect("create temp repo");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
    fs::write(root.join("a.txt"), b"base\n").expect("write base fixture");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base", "-q"]);
    fs::write(root.join("a.txt"), b"tracked\n").expect("write tracked fixture");
    git(root, &["stash", "push", "-q", "-m", "tracked only"]);
}

fn prepare_single_stash_repo(root: &Path) {
    fs::create_dir_all(root).expect("create temp repo");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
    fs::write(root.join("a.txt"), b"base\n").expect("write base fixture");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base", "-q"]);
    fs::write(root.join("a.txt"), b"one\n").expect("write stash fixture");
    git(root, &["stash", "push", "-q", "-m", "one"]);
}

fn prepare_stash_store_repo(root: &Path) -> String {
    fs::create_dir_all(root).expect("create temp repo");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
    fs::write(root.join("a.txt"), b"base\n").expect("write base fixture");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base", "-q"]);
    fs::write(root.join("a.txt"), b"one\n").expect("write stash store fixture");
    String::from_utf8(git(root, &["stash", "create", "store fixture"]))
        .expect("stash create oid is utf8")
        .trim()
        .to_string()
}

fn copy_dir(source: &Path, target: &Path) {
    fs::create_dir_all(target).expect("create copied directory");
    for entry in fs::read_dir(source).expect("read source directory") {
        let entry = entry.expect("read source entry");
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if entry.file_type().expect("entry file type").is_dir() {
            copy_dir(&source_path, &target_path);
        } else {
            fs::copy(&source_path, &target_path).expect("copy fixture file");
        }
    }
}

#[test]
fn stash_list_matches_upstream_git() {
    let root = unique_temp_dir("stash-list");
    let result = (|| {
        prepare_stash_repo(&root);
        for args in [
            vec!["stash", "list"],
            vec!["stash", "list", "--oneline"],
            vec!["stash", "list", "--oneline", "--abbrev=12"],
            vec!["stash", "list", "--oneline", "--no-abbrev"],
            vec!["stash", "list", "--format=%H"],
            vec!["stash", "list", "--format=%h"],
            vec!["stash", "list", "--format=%h", "--abbrev=12"],
            vec!["stash", "list", "--format=%h", "--abbrev="],
            vec!["stash", "list", "--format=%h", "--abbrev=bad"],
            vec!["stash", "list", "--format=%h", "--abbrev=0"],
            vec!["stash", "list", "--format=%h", "--abbrev=-1"],
            vec!["stash", "list", "--format=%h", "--abbrev=100"],
            vec!["stash", "list", "--format=%h", "--no-abbrev"],
            vec!["stash", "list", "--pretty=format:%H"],
            vec!["stash", "list", "--pretty=format:%h"],
            vec!["stash", "list", "--format=%T|%t|%P|%p|%f|%e|%b|%B"],
            vec!["stash", "list", "--abbrev=12", "--format=%T|%t|%P|%p|%f"],
            vec![
                "stash",
                "list",
                "--format=%gd|%d|%D|%m|%N|%S|%G?|%GT|%GG|%GS|%GK|%GF|%GP",
            ],
            vec!["stash", "list", "--format=%H %h %gd %gD %gs"],
            vec!["stash", "list", "--format=%s"],
            vec!["stash", "list", "--format=literal %% %n%gs"],
            vec!["stash", "list", "--pretty=format:%h %gd %gs"],
            vec!["stash", "list", "--format=%gn <%ge> %gN <%gE> %gs"],
            vec!["stash", "list", "--format=%gd"],
            vec!["stash", "list", "--format=%gD"],
            vec!["stash", "list", "--format=%gs"],
            vec!["stash", "list", "--pretty=format:%gs"],
            vec!["stash", "list", "--grep=one"],
            vec!["stash", "list", "--grep", "one"],
            vec!["stash", "list", "--grep=ONE", "-i"],
            vec!["stash", "list", "--grep=one", "--format=%gd %gs"],
            vec!["stash", "list", "--grep=one", "--max-count=1"],
            vec!["stash", "list", "--grep=one", "--invert-grep"],
            vec!["stash", "list", "--grep=one", "--grep=two"],
            vec!["stash", "list", "--grep=one", "--grep=two", "--all-match"],
            vec!["stash", "list", "--grep=.", "-F"],
            vec!["stash", "list", "--grep=.", "--basic-regexp"],
            vec!["stash", "list", "--grep=o.e", "--perl-regexp"],
            vec!["stash", "list", "-P", "--grep=o.e"],
            vec!["stash", "list", "--perl-regexp", "-F", "--grep=o.e"],
            vec!["stash", "list", "-F", "--perl-regexp", "--grep=o.e"],
            vec!["stash", "list", "--author=Example"],
            vec!["stash", "list", "--committer=Example"],
            vec!["stash", "list", "--author"],
            vec!["stash", "list", "--committer"],
            vec!["stash", "list", "--max-age=0"],
            vec!["stash", "list", "--min-age=0"],
            vec!["stash", "list", "--skip=1"],
            vec!["stash", "list", "--skip", "1"],
            vec!["stash", "list", "--skip=-1"],
            vec!["stash", "list", "--skip=0", "--max-count=1"],
            vec!["stash", "list", "--max-count=1", "--skip=1"],
            vec!["stash", "list", "--max-count=1"],
            vec!["stash", "list", "--max-count=-1"],
            vec!["stash", "list", "-1"],
            vec!["stash", "list", "-n", "1"],
            vec!["stash", "list", "-n1"],
            vec!["stash", "list", "-q", "--format=%gd|%s"],
            vec!["stash", "list", "--quiet", "--format=%gd|%s"],
            vec!["stash", "list", "--no-quiet", "--format=%gd|%s"],
            vec!["stash", "list", "--no-graph", "--format=%gd|%s"],
            vec!["stash", "list", "--expand-tabs", "--format=%gd|%s"],
            vec!["stash", "list", "--expand-tabs=0", "--format=%gd|%s"],
            vec!["stash", "list", "--expand-tabs=4", "--format=%gd|%s"],
            vec!["stash", "list", "--no-expand-tabs", "--format=%gd|%s"],
            vec!["stash", "list", "--no-decorate"],
            vec!["stash", "list", "--decorate"],
            vec!["stash", "list", "--decorate=no"],
            vec!["stash", "list", "--decorate=short"],
            vec!["stash", "list", "--decorate=full"],
            vec!["stash", "list", "--decorate=auto"],
            vec!["stash", "list", "--color", "--format=%gd|%s"],
            vec!["stash", "list", "--no-color", "--format=%gd|%s"],
            vec!["stash", "list", "--color=always", "--format=%gd|%s"],
            vec!["stash", "list", "--color=auto", "--format=%gd|%s"],
            vec!["stash", "list", "--color=never", "--format=%gd|%s"],
            vec!["stash", "list", "--color-moved", "--format=%gd|%s"],
            vec!["stash", "list", "--no-color-moved", "--format=%gd|%s"],
            vec!["stash", "list", "--color-moved=plain", "--format=%gd|%s"],
            vec!["stash", "list", "--color-moved=no", "--format=%gd|%s"],
            vec![
                "stash",
                "list",
                "--color-moved-ws=ignore-all-space",
                "--format=%gd|%s",
            ],
            vec![
                "stash",
                "list",
                "--color-moved-ws",
                "allow-indentation-change",
                "--format=%gd|%s",
            ],
            vec!["stash", "list", "--clear-decorations", "--format=%gd|%s"],
            vec!["stash", "list", "--no-decorate-refs", "--format=%gd|%s"],
            vec![
                "stash",
                "list",
                "--no-decorate-refs-exclude",
                "--format=%gd|%s",
            ],
            vec![
                "stash",
                "list",
                "--decorate-refs",
                "refs/stash",
                "--format=%gd|%s",
            ],
            vec![
                "stash",
                "list",
                "--decorate-refs=refs/stash",
                "--format=%gd|%s",
            ],
            vec![
                "stash",
                "list",
                "--decorate-refs-exclude",
                "refs/heads/*",
                "--format=%gd|%s",
            ],
            vec![
                "stash",
                "list",
                "--decorate-refs-exclude=refs/heads/*",
                "--format=%gd|%s",
            ],
            vec!["stash", "list", "--walk-reflogs"],
            vec!["stash", "list", "--no-walk"],
            vec!["stash", "list", "--no-walk=sorted"],
            vec!["stash", "list", "--no-walk=unsorted"],
            vec!["stash", "list", "--do-walk"],
            vec!["stash", "list", "--first-parent"],
            vec!["stash", "list", "--parents"],
            vec!["stash", "list", "--encoding=UTF-8", "--format=%gd|%s"],
            vec!["stash", "list", "--encoding", "UTF-8", "--format=%gd|%s"],
            vec!["stash", "list", "--no-notes", "--format=%gd|%s"],
            vec!["stash", "list", "--notes", "--format=%gd|%s"],
            vec!["stash", "list", "--show-notes", "--format=%gd|%s"],
            vec!["stash", "list", "--standard-notes", "--format=%gd|%s"],
            vec!["stash", "list", "--no-standard-notes", "--format=%gd|%s"],
            vec!["stash", "list", "--show-signature", "--format=%gd|%G?|%s"],
            vec![
                "stash",
                "list",
                "--no-show-signature",
                "--format=%gd|%G?|%s",
            ],
            vec!["stash", "list", "--source", "--format=%gd|%S|%s"],
            vec!["stash", "list", "--no-source", "--format=%gd|%S|%s"],
            vec!["stash", "list", "--use-mailmap", "--format=%gd|%s"],
            vec!["stash", "list", "--mailmap", "--format=%gd|%s"],
            vec!["stash", "list", "--no-use-mailmap", "--format=%gd|%s"],
            vec!["stash", "list", "--no-mailmap", "--format=%gd|%s"],
            vec!["stash", "list", "--no-patch", "--format=%gd|%s"],
            vec!["stash", "list", "--no-ext-diff", "--format=%gd|%s"],
            vec!["stash", "list", "--ext-diff", "--format=%gd|%s"],
            vec!["stash", "list", "--no-textconv", "--format=%gd|%s"],
            vec!["stash", "list", "--textconv", "--format=%gd|%s"],
            vec!["stash", "list", "--full-diff", "--format=%gd|%s"],
            vec!["stash", "list", "--no-renames", "--format=%gd|%s"],
            vec!["stash", "list", "--find-renames", "--format=%gd|%s"],
            vec!["stash", "list", "-M", "--format=%gd|%s"],
            vec!["stash", "list", "-M50%", "--format=%gd|%s"],
            vec!["stash", "list", "--find-renames=50%", "--format=%gd|%s"],
            vec!["stash", "list", "--find-copies", "--format=%gd|%s"],
            vec!["stash", "list", "-C", "--format=%gd|%s"],
            vec!["stash", "list", "--find-copies=50%", "--format=%gd|%s"],
            vec!["stash", "list", "--find-copies-harder", "--format=%gd|%s"],
            vec![
                "stash",
                "list",
                "--no-find-copies-harder",
                "--format=%gd|%s",
            ],
            vec!["stash", "list", "--relative", "--format=%gd|%s"],
            vec!["stash", "list", "--relative=subdir", "--format=%gd|%s"],
            vec!["stash", "list", "--no-relative", "--format=%gd|%s"],
            vec!["stash", "list", "--diff-merges=off", "--format=%gd|%s"],
            vec!["stash", "list", "--diff-merges", "off", "--format=%gd|%s"],
            vec!["stash", "list", "--diff-merges=none", "--format=%gd|%s"],
            vec!["stash", "list", "--no-diff-merges", "--format=%gd|%s"],
            vec!["stash", "list", "--minimal", "--format=%gd|%s"],
            vec!["stash", "list", "--patience", "--format=%gd|%s"],
            vec!["stash", "list", "--histogram", "--format=%gd|%s"],
            vec!["stash", "list", "--indent-heuristic", "--format=%gd|%s"],
            vec!["stash", "list", "--no-indent-heuristic", "--format=%gd|%s"],
            vec!["stash", "list", "--ignore-space-at-eol", "--format=%gd|%s"],
            vec!["stash", "list", "--ignore-cr-at-eol", "--format=%gd|%s"],
            vec!["stash", "list", "--ignore-space-change", "--format=%gd|%s"],
            vec!["stash", "list", "--ignore-all-space", "--format=%gd|%s"],
            vec!["stash", "list", "--ignore-blank-lines", "--format=%gd|%s"],
            vec!["stash", "list", "--function-context", "--format=%gd|%s"],
            vec!["stash", "list", "-W", "--format=%gd|%s"],
            vec!["stash", "list", "-w", "--format=%gd|%s"],
            vec!["stash", "list", "-b", "--format=%gd|%s"],
            vec!["stash", "list", "--no-prefix", "--format=%gd|%s"],
            vec!["stash", "list", "--default-prefix", "--format=%gd|%s"],
            vec!["stash", "list", "--full-index", "--format=%gd|%s"],
            vec!["stash", "list", "--break-rewrites", "--format=%gd|%s"],
            vec!["stash", "list", "--irreversible-delete", "--format=%gd|%s"],
            vec!["stash", "list", "-B", "--format=%gd|%s"],
            vec!["stash", "list", "-D", "--format=%gd|%s"],
            vec!["stash", "list", "-m", "--format=%gd|%s"],
            vec!["stash", "list", "-s", "--format=%gd|%s"],
            vec!["stash", "list", "--src-prefix=old/", "--format=%gd|%s"],
            vec!["stash", "list", "--src-prefix", "old/", "--format=%gd|%s"],
            vec!["stash", "list", "--dst-prefix=new/", "--format=%gd|%s"],
            vec!["stash", "list", "--dst-prefix", "new/", "--format=%gd|%s"],
            vec![
                "stash",
                "list",
                "--output-indicator-new=>",
                "--format=%gd|%s",
            ],
            vec![
                "stash",
                "list",
                "--output-indicator-old=<",
                "--format=%gd|%s",
            ],
            vec![
                "stash",
                "list",
                "--output-indicator-context=.",
                "--format=%gd|%s",
            ],
            vec![
                "stash",
                "list",
                "--ws-error-highlight=all",
                "--format=%gd|%s",
            ],
            vec![
                "stash",
                "list",
                "--ws-error-highlight",
                "old,new",
                "--format=%gd|%s",
            ],
            vec!["stash", "list", "--submodule", "--format=%gd|%s"],
            vec!["stash", "list", "--submodule=short", "--format=%gd|%s"],
            vec!["stash", "list", "--submodule=log", "--format=%gd|%s"],
            vec!["stash", "list", "--submodule=diff", "--format=%gd|%s"],
            vec!["stash", "list", "--ignore-submodules", "--format=%gd|%s"],
            vec![
                "stash",
                "list",
                "--ignore-submodules=none",
                "--format=%gd|%s",
            ],
            vec![
                "stash",
                "list",
                "--ignore-submodules=dirty",
                "--format=%gd|%s",
            ],
            vec!["stash", "list", "--ita-visible-in-index", "--format=%gd|%s"],
            vec![
                "stash",
                "list",
                "--ita-invisible-in-index",
                "--format=%gd|%s",
            ],
            vec!["stash", "list", "--pickaxe-all", "--format=%gd|%s"],
            vec!["stash", "list", "--pickaxe-regex", "--format=%gd|%s"],
            vec![
                "stash",
                "list",
                "--pickaxe-all",
                "--pickaxe-regex",
                "--format=%gd|%s",
            ],
            vec!["stash", "list", "--merges"],
            vec!["stash", "list", "--no-merges"],
            vec!["stash", "list", "--min-parents=0"],
            vec!["stash", "list", "--min-parents=2"],
            vec!["stash", "list", "--min-parents=3"],
            vec!["stash", "list", "--min-parents=-1"],
            vec!["stash", "list", "--max-parents=1"],
            vec!["stash", "list", "--max-parents=2"],
            vec!["stash", "list", "--max-parents=-1"],
            vec!["stash", "list", "--no-min-parents"],
            vec!["stash", "list", "--no-max-parents"],
            vec!["stash", "list", "--max-parents=1", "--no-max-parents"],
            vec!["stash", "list", "--full-history"],
            vec!["stash", "list", "--dense"],
            vec!["stash", "list", "--sparse"],
            vec!["stash", "list", "--remove-empty"],
            vec!["stash", "list", "--left-right"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(
                actual, expected,
                "git-rs stash output differed for {args:?}"
            );
        }
        for args in [
            ["stash", "list", "--reverse"].as_slice(),
            ["stash", "list", "--oneline", "--reverse"].as_slice(),
            ["stash", "list", "--graph"].as_slice(),
            ["stash", "list", "--decorate=bad"].as_slice(),
            ["stash", "list", "--no-graph=value"].as_slice(),
            ["stash", "list", "--expand-tabs=bad"].as_slice(),
            ["stash", "list", "--expand-tabs=-1"].as_slice(),
            ["stash", "list", "--no-expand-tabs=value"].as_slice(),
            ["stash", "list", "--perl-regexp=value"].as_slice(),
            ["stash", "list", "--basic-regexp=value"].as_slice(),
            ["stash", "list", "--extended-regexp=value"].as_slice(),
            ["stash", "list", "--fixed-strings=value"].as_slice(),
            ["stash", "list", "--regexp-ignore-case=value"].as_slice(),
            ["stash", "list", "--all-match=value"].as_slice(),
            ["stash", "list", "--invert-grep=value"].as_slice(),
            ["stash", "list", "--no-perl-regexp"].as_slice(),
            ["stash", "list", "--no-basic-regexp"].as_slice(),
            ["stash", "list", "--no-extended-regexp"].as_slice(),
            ["stash", "list", "--no-fixed-strings"].as_slice(),
            ["stash", "list", "--no-regexp-ignore-case"].as_slice(),
            ["stash", "list", "--no-all-match"].as_slice(),
            ["stash", "list", "--no-invert-grep"].as_slice(),
            ["stash", "list", "--no-grep"].as_slice(),
            ["stash", "list", "--no-walk=bad"].as_slice(),
            ["stash", "list", "--walk-reflogs=value"].as_slice(),
            ["stash", "list", "--do-walk=value"].as_slice(),
            ["stash", "list", "--first-parent=value"].as_slice(),
            ["stash", "list", "--parents=value"].as_slice(),
            ["stash", "list", "--quiet=value"].as_slice(),
            ["stash", "list", "--no-quiet=value"].as_slice(),
            ["stash", "list", "--no-decorate=false"].as_slice(),
            ["stash", "list", "--color=bad"].as_slice(),
            ["stash", "list", "--color-moved=bad"].as_slice(),
            ["stash", "list", "--color-moved-ws=bad"].as_slice(),
            ["stash", "list", "--clear-decorations=value"].as_slice(),
            ["stash", "list", "--no-decorate-refs=value"].as_slice(),
            ["stash", "list", "--no-decorate-refs-exclude=value"].as_slice(),
            ["stash", "list", "--use-mailmap=value"].as_slice(),
            ["stash", "list", "--mailmap=value"].as_slice(),
            ["stash", "list", "--no-use-mailmap=value"].as_slice(),
            ["stash", "list", "--no-mailmap=value"].as_slice(),
            ["stash", "list", "--source=value"].as_slice(),
            ["stash", "list", "--no-source=value"].as_slice(),
            ["stash", "list", "--show-signature=value"].as_slice(),
            ["stash", "list", "--no-show-signature=value"].as_slice(),
            ["stash", "list", "--no-notes=value"].as_slice(),
            ["stash", "list", "--standard-notes=value"].as_slice(),
            ["stash", "list", "--no-standard-notes=value"].as_slice(),
            ["stash", "list", "--output-indicator-new=abc"].as_slice(),
            ["stash", "list", "--ws-error-highlight=bad"].as_slice(),
            ["stash", "list", "--submodule=bad"].as_slice(),
            ["stash", "list", "--ignore-submodules=bad"].as_slice(),
            ["stash", "list", "--children"].as_slice(),
            ["stash", "list", "--cherry-pick"].as_slice(),
            ["stash", "list", "--ancestry-path"].as_slice(),
            ["stash", "list", "--topo-order"].as_slice(),
            ["stash", "list", "--date-order"].as_slice(),
            ["stash", "list", "--author-date-order"].as_slice(),
            ["stash", "list", "--simplify-by-decoration"].as_slice(),
            ["stash", "list", "--simplify-merges"].as_slice(),
            ["stash", "list", "--skip=bad"].as_slice(),
            ["stash", "list", "--skip"].as_slice(),
            ["stash", "list", "--max-count=bad"].as_slice(),
            ["stash", "list", "--max-count"].as_slice(),
            ["stash", "list", "--min-parents=bad"].as_slice(),
            ["stash", "list", "--max-parents=bad"].as_slice(),
            ["stash", "list", "--author=["].as_slice(),
            ["stash", "list", "--committer=["].as_slice(),
            ["stash", "list", "--max-age=bad"].as_slice(),
            ["stash", "list", "--min-age=bad"].as_slice(),
            ["stash", "list", "--max-age"].as_slice(),
            ["stash", "list", "--min-age"].as_slice(),
        ] {
            let expected = run_output("git", &root, args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, args);
            assert_same_output(actual, expected, args);
        }

        let identity = root.join("identity");
        prepare_stash_identity_repo(&identity);
        for args in [
            vec!["stash", "list", "--author=Author One"],
            vec!["stash", "list", "--author", "Author Two"],
            vec!["stash", "list", "--author=author1@example"],
            vec!["stash", "list", "--author=AUTHOR TWO", "-i"],
            vec!["stash", "list", "--committer=Committer One"],
            vec!["stash", "list", "--committer", "committer2@example"],
            vec!["stash", "list", "--committer=COMMITTER TWO", "-i"],
            vec![
                "stash",
                "list",
                "--format=%gd|%an|%ae|%al|%aN|%aE|%aL|%cn|%ce|%cl|%cN|%cE|%cL|%at|%ct|%s",
            ],
            vec![
                "stash",
                "list",
                "--format=%gd|%ad|%ai|%aI|%as|%aD|%cd|%ci|%cI|%cs|%cD",
            ],
            vec!["stash", "list", "--date=iso", "--format=%ad|%cd"],
            vec!["stash", "list", "--date", "short", "--format=%ad|%cd"],
            vec!["stash", "list", "--format=%gd%x09%C(red)%s%Creset"],
            vec![
                "stash",
                "list",
                "--author=Author One",
                "--committer=Committer One",
            ],
            vec![
                "stash",
                "list",
                "--author=Author One",
                "--committer=Committer Two",
            ],
            vec!["stash", "list", "--author=Missing"],
            vec!["stash", "list", "--committer=Missing"],
        ] {
            let expected = git(&identity, &args);
            let actual = git_rs(&identity, &args);
            assert_eq!(
                actual, expected,
                "git-rs stash identity filter output differed for {args:?}"
            );
        }

        let ages = root.join("ages");
        prepare_stash_age_repo(&ages);
        for args in [
            vec!["stash", "list", "--max-age=1500"],
            vec!["stash", "list", "--max-age", "1500"],
            vec!["stash", "list", "--min-age=1500"],
            vec!["stash", "list", "--min-age", "1500"],
            vec!["stash", "list", "--max-age=-1"],
            vec!["stash", "list", "--min-age=-1"],
            vec!["stash", "list", "--max-age=0"],
            vec!["stash", "list", "--min-age=0"],
            vec!["stash", "list", "--max-age=1500", "--min-age=1500"],
            vec!["stash", "list", "--max-age=1500", "--max-count=1"],
            vec!["stash", "list", "--min-age=1500", "--skip=1"],
            vec!["stash", "list", "--since=@1500 +0000"],
            vec!["stash", "list", "--after=@1500 +0000"],
            vec!["stash", "list", "--until=@1500 +0000"],
            vec!["stash", "list", "--before=@1500 +0000"],
            vec!["stash", "list", "--since", "@1500 +0000"],
            vec!["stash", "list", "--until", "@1500 +0000"],
            vec!["stash", "list", "--since=1970-01-01 00:25:00 +0000"],
            vec!["stash", "list", "--until=1970-01-01T00:25:00 +0000"],
            vec!["stash", "list", "--since=@1500 +0000", "--max-count=1"],
            vec!["stash", "list", "--until=@1500 +0000", "--skip=1"],
            vec!["stash", "list", "--date=default", "--format=%gd|%gD"],
            vec!["stash", "list", "--date=iso", "--format=%gd|%gD"],
            vec!["stash", "list", "--date=short", "--format=%gd|%gD"],
            vec!["stash", "list", "--date=raw", "--format=%gd|%gD"],
        ] {
            let expected = git(&ages, &args);
            let actual = git_rs(&ages, &args);
            assert_eq!(
                actual, expected,
                "git-rs stash age filter output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_create_matches_upstream_git() {
    let root = unique_temp_dir("stash-create");
    let result = (|| {
        let clean_upstream = root.join("clean-upstream");
        let clean_actual = root.join("clean-actual");
        prepare_stash_create_repo(&clean_upstream, "clean");
        prepare_stash_create_repo(&clean_actual, "clean");
        let clean_args = ["stash", "create"];
        let expected = run_output_with_fixed_identity("git", &clean_upstream, &clean_args);
        let actual_output = run_output_with_fixed_identity(
            env!("CARGO_BIN_EXE_git-rs"),
            &clean_actual,
            &clean_args,
        );
        assert_same_output(actual_output, expected, &clean_args);

        for (name, setup, args) in [
            ("unstaged", "unstaged", vec!["stash", "create"]),
            ("message", "unstaged", vec!["stash", "create", "saved work"]),
            (
                "multi-message",
                "unstaged",
                vec!["stash", "create", "one", "two"],
            ),
            ("staged", "staged", vec!["stash", "create"]),
            (
                "staged-and-unstaged",
                "staged-and-unstaged",
                vec!["stash", "create"],
            ),
            ("deleted", "deleted", vec!["stash", "create"]),
            ("detached", "detached", vec!["stash", "create"]),
            ("unborn", "unborn", vec!["stash", "create"]),
        ] {
            let template = root.join(format!("{name}-template"));
            prepare_stash_create_repo(&template, setup);
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            let expected = run_output_with_fixed_identity("git", &upstream, &args);
            let actual_output =
                run_output_with_fixed_identity(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_eq!(
                actual_output.status.code(),
                expected.status.code(),
                "status differed for stash create case {name} {args:?}\nactual stdout:\n{}\nactual stderr:\n{}\nexpected stdout:\n{}\nexpected stderr:\n{}",
                String::from_utf8_lossy(&actual_output.stdout),
                String::from_utf8_lossy(&actual_output.stderr),
                String::from_utf8_lossy(&expected.stdout),
                String::from_utf8_lossy(&expected.stderr),
            );
            assert_eq!(
                actual_output.stdout, expected.stdout,
                "stdout differed for stash create case {name} {args:?}"
            );
            assert_eq!(
                actual_output.stderr, expected.stderr,
                "stderr differed for stash create case {name} {args:?}"
            );

            for check_args in [
                vec!["status", "--short"],
                vec!["show-ref", "--exists", "refs/stash"],
            ] {
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                assert_same_output(actual_output, expected, &check_args);
            }

            let oid =
                String::from_utf8(run_output_with_fixed_identity("git", &upstream, &args).stdout)
                    .expect("stash create oid is utf8")
                    .trim()
                    .to_string();
            if oid.is_empty() {
                continue;
            }
            for check_args in [
                vec!["cat-file", "-p", oid.as_str()],
                vec!["stash", "store", "-m", "created", oid.as_str()],
                vec!["stash", "show", "--stat", "-p"],
            ] {
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                assert_same_output(actual_output, expected, &check_args);
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn prepare_stash_create_repo(root: &Path, setup: &str) {
    fs::create_dir_all(root).expect("create temp repo");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
    if setup == "unborn" {
        fs::write(root.join("a.txt"), b"unborn\n").expect("write unborn fixture");
        git(root, &["add", "a.txt"]);
        return;
    }
    fs::write(root.join("a.txt"), b"base\n").expect("write base fixture");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base", "-q"]);
    match setup {
        "clean" => {}
        "unstaged" => {
            fs::write(root.join("a.txt"), b"worktree\n").expect("write unstaged fixture");
        }
        "untracked" => {
            fs::write(root.join("u.txt"), b"untracked\n").expect("write untracked fixture");
        }
        "ignored" => {
            fs::write(root.join(".gitignore"), b"*.log\n").expect("write gitignore fixture");
            fs::write(root.join("ignored.log"), b"ignored\n").expect("write ignored fixture");
            fs::write(root.join("u.txt"), b"untracked\n").expect("write untracked fixture");
        }
        "tracked-and-untracked" => {
            fs::write(root.join("a.txt"), b"worktree\n").expect("write unstaged fixture");
            fs::write(root.join("u.txt"), b"untracked\n").expect("write untracked fixture");
        }
        "two-tracked" => {
            fs::write(root.join("b.txt"), b"base\n").expect("write second base fixture");
            git(root, &["add", "b.txt"]);
            git(root, &["commit", "-m", "second", "-q"]);
            fs::write(root.join("a.txt"), b"worktree a\n").expect("write first worktree fixture");
            fs::write(root.join("b.txt"), b"worktree b\n").expect("write second worktree fixture");
        }
        "two-tracked-pathspec-file" => {
            fs::write(root.join("b.txt"), b"base\n").expect("write second base fixture");
            git(root, &["add", "b.txt"]);
            git(root, &["commit", "-m", "second", "-q"]);
            fs::write(root.join("a.txt"), b"worktree a\n").expect("write first worktree fixture");
            fs::write(root.join("b.txt"), b"worktree b\n").expect("write second worktree fixture");
            fs::write(root.join("pathspecs"), b"a.txt\n").expect("write pathspec fixture");
        }
        "two-tracked-pathspec-nul" => {
            fs::write(root.join("b.txt"), b"base\n").expect("write second base fixture");
            git(root, &["add", "b.txt"]);
            git(root, &["commit", "-m", "second", "-q"]);
            fs::write(root.join("a.txt"), b"worktree a\n").expect("write first worktree fixture");
            fs::write(root.join("b.txt"), b"worktree b\n").expect("write second worktree fixture");
            fs::write(root.join("pathspecs"), b"a.txt\0").expect("write pathspec fixture");
        }
        "two-tracked-staged" => {
            fs::write(root.join("b.txt"), b"base\n").expect("write second base fixture");
            git(root, &["add", "b.txt"]);
            git(root, &["commit", "-m", "second", "-q"]);
            fs::write(root.join("a.txt"), b"staged a\n").expect("write first staged fixture");
            git(root, &["add", "a.txt"]);
            fs::write(root.join("b.txt"), b"staged b\n").expect("write second staged fixture");
            git(root, &["add", "b.txt"]);
        }
        "two-tracked-and-untracked" => {
            fs::write(root.join("b.txt"), b"base\n").expect("write second base fixture");
            git(root, &["add", "b.txt"]);
            git(root, &["commit", "-m", "second", "-q"]);
            fs::write(root.join("a.txt"), b"worktree a\n").expect("write first worktree fixture");
            fs::write(root.join("b.txt"), b"worktree b\n").expect("write second worktree fixture");
            fs::write(root.join("u.txt"), b"untracked\n").expect("write untracked fixture");
        }
        "staged" => {
            fs::write(root.join("a.txt"), b"staged\n").expect("write staged fixture");
            git(root, &["add", "a.txt"]);
        }
        "staged-and-unstaged" => {
            fs::write(root.join("a.txt"), b"staged\n").expect("write staged fixture");
            git(root, &["add", "a.txt"]);
            fs::write(root.join("a.txt"), b"worktree\n").expect("write worktree fixture");
        }
        "staged-and-unstaged-separate" => {
            fs::write(root.join("b.txt"), b"base b\n").expect("write second base fixture");
            git(root, &["add", "b.txt"]);
            git(root, &["commit", "-m", "second", "-q"]);
            fs::write(root.join("a.txt"), b"staged\n").expect("write staged fixture");
            git(root, &["add", "a.txt"]);
            fs::write(root.join("b.txt"), b"worktree\n").expect("write worktree fixture");
        }
        "deleted" => {
            fs::remove_file(root.join("a.txt")).expect("remove tracked fixture");
        }
        "detached" => {
            git(root, &["checkout", "-q", "--detach", "HEAD"]);
            fs::write(root.join("a.txt"), b"detached\n").expect("write detached fixture");
        }
        other => panic!("unknown stash create setup {other}"),
    }
}

#[test]
fn stash_push_matches_upstream_git() {
    let root = unique_temp_dir("stash-push");
    let result = (|| {
        for (name, setup, args) in [
            ("default-subcommand", "unstaged", vec!["stash"]),
            ("clean", "clean", vec!["stash", "push"]),
            ("unstaged", "unstaged", vec!["stash", "push"]),
            ("message", "unstaged", vec!["stash", "push", "-m", "saved"]),
            (
                "include-untracked",
                "untracked",
                vec!["stash", "push", "-u"],
            ),
            (
                "include-untracked-long",
                "tracked-and-untracked",
                vec!["stash", "push", "--include-untracked"],
            ),
            ("all", "ignored", vec!["stash", "push", "-a"]),
            ("all-long", "ignored", vec!["stash", "push", "--all"]),
            (
                "no-include-untracked",
                "untracked",
                vec!["stash", "push", "-u", "--no-include-untracked"],
            ),
            ("no-all", "ignored", vec!["stash", "push", "-a", "--no-all"]),
            (
                "message-equals",
                "unstaged",
                vec!["stash", "push", "--message=saved2"],
            ),
            (
                "message-short",
                "unstaged",
                vec!["stash", "push", "-msaved3"],
            ),
            (
                "quiet",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "quiet"],
            ),
            (
                "no-quiet",
                "unstaged",
                vec!["stash", "push", "-q", "--no-quiet"],
            ),
            ("no-patch", "unstaged", vec!["stash", "push", "--no-patch"]),
            (
                "patch-no-patch",
                "unstaged",
                vec!["stash", "push", "--patch", "--no-patch"],
            ),
            (
                "no-message",
                "unstaged",
                vec!["stash", "push", "-m", "ignored", "--no-message"],
            ),
            (
                "no-message-value",
                "unstaged",
                vec!["stash", "push", "--no-message=value"],
            ),
            (
                "auto-advance",
                "unstaged",
                vec!["stash", "push", "--auto-advance"],
            ),
            (
                "auto-advance-value",
                "unstaged",
                vec!["stash", "push", "--auto-advance=false"],
            ),
            (
                "no-auto-advance",
                "unstaged",
                vec!["stash", "push", "--no-auto-advance"],
            ),
            (
                "no-auto-advance-no-patch",
                "unstaged",
                vec![
                    "stash",
                    "push",
                    "--patch",
                    "--no-auto-advance",
                    "--no-patch",
                ],
            ),
            ("unified", "unstaged", vec!["stash", "push", "--unified=1"]),
            ("unified-short", "unstaged", vec!["stash", "push", "-U1"]),
            (
                "unified-missing",
                "unstaged",
                vec!["stash", "push", "--unified"],
            ),
            (
                "unified-invalid",
                "unstaged",
                vec!["stash", "push", "--unified=bad"],
            ),
            (
                "inter-hunk-context",
                "unstaged",
                vec!["stash", "push", "--inter-hunk-context=1"],
            ),
            (
                "inter-hunk-context-missing",
                "unstaged",
                vec!["stash", "push", "--inter-hunk-context"],
            ),
            (
                "inter-hunk-context-invalid",
                "unstaged",
                vec!["stash", "push", "--inter-hunk-context=bad"],
            ),
            (
                "keep-index-staged",
                "staged",
                vec!["stash", "push", "--keep-index"],
            ),
            ("keep-index-short", "staged", vec!["stash", "push", "-k"]),
            (
                "keep-index-mixed",
                "staged-and-unstaged",
                vec!["stash", "push", "--keep-index"],
            ),
            (
                "keep-index-untracked",
                "tracked-and-untracked",
                vec!["stash", "push", "--keep-index", "-u"],
            ),
            (
                "no-keep-index",
                "staged",
                vec!["stash", "push", "--keep-index", "--no-keep-index"],
            ),
            ("staged-option", "staged", vec!["stash", "push", "--staged"]),
            ("staged-option-short", "staged", vec!["stash", "push", "-S"]),
            (
                "staged-option-with-separate-unstaged",
                "staged-and-unstaged-separate",
                vec!["stash", "push", "--staged"],
            ),
            (
                "staged-option-with-same-path-unstaged",
                "staged-and-unstaged",
                vec!["stash", "push", "--staged"],
            ),
            (
                "no-staged-option",
                "staged",
                vec!["stash", "push", "--staged", "--no-staged"],
            ),
            (
                "staged-option-no-staged-changes",
                "unstaged",
                vec!["stash", "push", "--staged"],
            ),
            (
                "staged-option-untracked-only",
                "untracked",
                vec!["stash", "push", "--staged"],
            ),
            (
                "staged-option-rejects-untracked",
                "staged",
                vec!["stash", "push", "--staged", "-u"],
            ),
            (
                "pathspec-tracked",
                "two-tracked",
                vec!["stash", "push", "--", "a.txt"],
            ),
            (
                "pathspec-from-file",
                "two-tracked-pathspec-file",
                vec!["stash", "push", "--pathspec-from-file=pathspecs"],
            ),
            (
                "pathspec-file-nul",
                "two-tracked-pathspec-nul",
                vec![
                    "stash",
                    "push",
                    "--pathspec-file-nul",
                    "--pathspec-from-file",
                    "pathspecs",
                ],
            ),
            (
                "pathspec-from-file-mixed",
                "two-tracked-pathspec-file",
                vec!["stash", "push", "--pathspec-from-file=pathspecs", "a.txt"],
            ),
            (
                "pathspec-file-nul-without-file",
                "two-tracked",
                vec!["stash", "push", "--pathspec-file-nul", "a.txt"],
            ),
            (
                "pathspec-from-file-reset",
                "two-tracked-pathspec-file",
                vec![
                    "stash",
                    "push",
                    "--pathspec-from-file=pathspecs",
                    "--no-pathspec-from-file",
                ],
            ),
            (
                "pathspec-untracked",
                "two-tracked-and-untracked",
                vec!["stash", "push", "-u", "--", "a.txt", "u.txt"],
            ),
            (
                "pathspec-preserves-unselected-staged",
                "two-tracked-staged",
                vec!["stash", "push", "--", "a.txt"],
            ),
            ("staged", "staged", vec!["stash", "push"]),
            (
                "staged-and-unstaged",
                "staged-and-unstaged",
                vec!["stash", "push"],
            ),
            ("deleted", "deleted", vec!["stash", "push"]),
            ("detached", "detached", vec!["stash", "push"]),
            ("unborn", "unborn", vec!["stash", "push"]),
        ] {
            let template = root.join(format!("{name}-template"));
            prepare_stash_create_repo(&template, setup);
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            let expected = run_output_with_fixed_identity("git", &upstream, &args);
            let actual_output =
                run_output_with_fixed_identity(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_eq!(
                actual_output.status.code(),
                expected.status.code(),
                "status differed for stash push case {name} {args:?}\nactual stdout:\n{}\nactual stderr:\n{}\nexpected stdout:\n{}\nexpected stderr:\n{}",
                String::from_utf8_lossy(&actual_output.stdout),
                String::from_utf8_lossy(&actual_output.stderr),
                String::from_utf8_lossy(&expected.stdout),
                String::from_utf8_lossy(&expected.stderr),
            );
            assert_eq!(
                actual_output.stdout, expected.stdout,
                "stdout differed for stash push case {name} {args:?}"
            );
            assert_eq!(
                actual_output.stderr, expected.stderr,
                "stderr differed for stash push case {name} {args:?}"
            );

            for check_args in [
                vec!["status", "--short"],
                vec!["show-ref", "--exists", "refs/stash"],
                vec!["stash", "list", "--format=%gs"],
            ] {
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                assert_eq!(
                    actual_output.status.code(),
                    expected.status.code(),
                    "post-state status differed for stash push case {name} {args:?} check {check_args:?}"
                );
                assert_eq!(
                    actual_output.stdout, expected.stdout,
                    "post-state stdout differed for stash push case {name} {args:?} check {check_args:?}"
                );
                assert_eq!(
                    actual_output.stderr, expected.stderr,
                    "post-state stderr differed for stash push case {name} {args:?} check {check_args:?}"
                );
            }
            let stash_exists =
                run_output("git", &upstream, &["show-ref", "--exists", "refs/stash"])
                    .status
                    .success();
            if stash_exists {
                for check_args in [
                    vec!["rev-parse", "refs/stash"],
                    vec!["cat-file", "-p", "refs/stash"],
                    vec!["stash", "show", "--stat", "-p"],
                    vec!["stash", "show", "--include-untracked", "--stat", "-p"],
                    vec!["stash", "show", "--only-untracked", "--name-only"],
                ] {
                    let expected = run_output("git", &upstream, &check_args);
                    let actual_output =
                        run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                    assert_same_output(actual_output, expected, &check_args);
                }
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_save_matches_upstream_git() {
    let root = unique_temp_dir("stash-save");
    let result = (|| {
        for (name, setup, args) in [
            ("clean", "clean", vec!["stash", "save"]),
            ("unstaged", "unstaged", vec!["stash", "save"]),
            ("message", "unstaged", vec!["stash", "save", "saved"]),
            (
                "multi-message",
                "unstaged",
                vec!["stash", "save", "saved", "work"],
            ),
            (
                "message-option",
                "unstaged",
                vec!["stash", "save", "-m", "saved"],
            ),
            (
                "message-equals",
                "unstaged",
                vec!["stash", "save", "--message=saved2"],
            ),
            (
                "message-short",
                "unstaged",
                vec!["stash", "save", "-msaved3"],
            ),
            (
                "positional-overrides-message",
                "unstaged",
                vec!["stash", "save", "-m", "ignored", "saved"],
            ),
            (
                "include-untracked",
                "untracked",
                vec!["stash", "save", "-u"],
            ),
            (
                "include-untracked-after-message",
                "tracked-and-untracked",
                vec!["stash", "save", "saved", "-u"],
            ),
            ("all", "ignored", vec!["stash", "save", "-a"]),
            ("all-long", "ignored", vec!["stash", "save", "--all"]),
            (
                "no-include-untracked",
                "untracked",
                vec!["stash", "save", "-u", "--no-include-untracked"],
            ),
            ("no-all", "ignored", vec!["stash", "save", "-a", "--no-all"]),
            (
                "literal-dashdash-message",
                "unstaged",
                vec!["stash", "save", "--", "-dash"],
            ),
            ("quiet", "unstaged", vec!["stash", "save", "-q", "quiet"]),
            (
                "no-quiet",
                "unstaged",
                vec!["stash", "save", "-q", "--no-quiet"],
            ),
            ("no-patch", "unstaged", vec!["stash", "save", "--no-patch"]),
            (
                "patch-no-patch",
                "unstaged",
                vec!["stash", "save", "--patch", "--no-patch"],
            ),
            (
                "no-message",
                "unstaged",
                vec!["stash", "save", "-m", "ignored", "--no-message"],
            ),
            (
                "no-message-value",
                "unstaged",
                vec!["stash", "save", "--no-message=value"],
            ),
            (
                "auto-advance",
                "unstaged",
                vec!["stash", "save", "--auto-advance"],
            ),
            (
                "auto-advance-value",
                "unstaged",
                vec!["stash", "save", "--auto-advance=false"],
            ),
            (
                "no-auto-advance",
                "unstaged",
                vec!["stash", "save", "--no-auto-advance"],
            ),
            (
                "no-auto-advance-no-patch",
                "unstaged",
                vec![
                    "stash",
                    "save",
                    "--patch",
                    "--no-auto-advance",
                    "--no-patch",
                ],
            ),
            ("unified", "unstaged", vec!["stash", "save", "--unified=1"]),
            ("unified-short", "unstaged", vec!["stash", "save", "-U1"]),
            (
                "unified-missing",
                "unstaged",
                vec!["stash", "save", "--unified"],
            ),
            (
                "unified-invalid",
                "unstaged",
                vec!["stash", "save", "--unified=bad"],
            ),
            (
                "inter-hunk-context",
                "unstaged",
                vec!["stash", "save", "--inter-hunk-context=1"],
            ),
            (
                "inter-hunk-context-missing",
                "unstaged",
                vec!["stash", "save", "--inter-hunk-context"],
            ),
            (
                "inter-hunk-context-invalid",
                "unstaged",
                vec!["stash", "save", "--inter-hunk-context=bad"],
            ),
            (
                "keep-index-staged",
                "staged",
                vec!["stash", "save", "--keep-index"],
            ),
            ("keep-index-short", "staged", vec!["stash", "save", "-k"]),
            (
                "keep-index-mixed",
                "staged-and-unstaged",
                vec!["stash", "save", "--keep-index"],
            ),
            (
                "no-keep-index",
                "staged",
                vec!["stash", "save", "--keep-index", "--no-keep-index"],
            ),
            ("staged-option", "staged", vec!["stash", "save", "--staged"]),
            ("staged-option-short", "staged", vec!["stash", "save", "-S"]),
            (
                "staged-option-with-separate-unstaged",
                "staged-and-unstaged-separate",
                vec!["stash", "save", "--staged"],
            ),
            (
                "staged-option-with-same-path-unstaged",
                "staged-and-unstaged",
                vec!["stash", "save", "--staged"],
            ),
            (
                "no-staged-option",
                "staged",
                vec!["stash", "save", "--staged", "--no-staged"],
            ),
            (
                "staged-option-no-staged-changes",
                "unstaged",
                vec!["stash", "save", "--staged"],
            ),
            (
                "staged-option-rejects-untracked",
                "staged",
                vec!["stash", "save", "--staged", "-u"],
            ),
            ("staged", "staged", vec!["stash", "save"]),
            (
                "staged-and-unstaged",
                "staged-and-unstaged",
                vec!["stash", "save"],
            ),
            ("deleted", "deleted", vec!["stash", "save"]),
            ("detached", "detached", vec!["stash", "save"]),
            ("unborn", "unborn", vec!["stash", "save"]),
        ] {
            let template = root.join(format!("{name}-template"));
            prepare_stash_create_repo(&template, setup);
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            let expected = run_output_with_fixed_identity("git", &upstream, &args);
            let actual_output =
                run_output_with_fixed_identity(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_eq!(
                actual_output.status.code(),
                expected.status.code(),
                "status differed for stash save case {name} {args:?}\nactual stdout:\n{}\nactual stderr:\n{}\nexpected stdout:\n{}\nexpected stderr:\n{}",
                String::from_utf8_lossy(&actual_output.stdout),
                String::from_utf8_lossy(&actual_output.stderr),
                String::from_utf8_lossy(&expected.stdout),
                String::from_utf8_lossy(&expected.stderr),
            );
            assert_eq!(
                actual_output.stdout, expected.stdout,
                "stdout differed for stash save case {name} {args:?}"
            );
            assert_eq!(
                actual_output.stderr, expected.stderr,
                "stderr differed for stash save case {name} {args:?}"
            );

            for check_args in [
                vec!["status", "--short"],
                vec!["show-ref", "--exists", "refs/stash"],
                vec!["stash", "list", "--format=%gs"],
            ] {
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                assert_same_output(actual_output, expected, &check_args);
            }
            let stash_exists =
                run_output("git", &upstream, &["show-ref", "--exists", "refs/stash"])
                    .status
                    .success();
            if stash_exists {
                for check_args in [
                    vec!["rev-parse", "refs/stash"],
                    vec!["cat-file", "-p", "refs/stash"],
                    vec!["stash", "show", "--stat", "-p"],
                    vec!["stash", "show", "--include-untracked", "--stat", "-p"],
                    vec!["stash", "show", "--only-untracked", "--name-only"],
                ] {
                    let expected = run_output("git", &upstream, &check_args);
                    let actual_output =
                        run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                    assert_same_output(actual_output, expected, &check_args);
                }
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_apply_matches_upstream_git_for_clean_head() {
    let root = unique_temp_dir("stash-apply");
    let result = (|| {
        for (name, setup, push_args, apply_args) in [
            (
                "unstaged",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "unstaged"],
                vec!["stash", "apply"],
            ),
            (
                "staged-default",
                "staged",
                vec!["stash", "push", "-q", "-m", "staged"],
                vec!["stash", "apply"],
            ),
            (
                "staged-index",
                "staged",
                vec!["stash", "push", "-q", "-m", "staged"],
                vec!["stash", "apply", "--index"],
            ),
            (
                "staged-and-unstaged",
                "staged-and-unstaged",
                vec!["stash", "push", "-q", "-m", "mixed"],
                vec!["stash", "apply"],
            ),
            (
                "staged-and-unstaged-index",
                "staged-and-unstaged",
                vec!["stash", "push", "-q", "-m", "mixed"],
                vec!["stash", "apply", "--index"],
            ),
            (
                "deleted",
                "deleted",
                vec!["stash", "push", "-q", "-m", "deleted"],
                vec!["stash", "apply"],
            ),
            (
                "untracked",
                "tracked-and-untracked",
                vec!["stash", "push", "-q", "-u", "-m", "untracked"],
                vec!["stash", "apply"],
            ),
            (
                "all",
                "ignored",
                vec!["stash", "push", "-q", "-a", "-m", "all"],
                vec!["stash", "apply"],
            ),
            (
                "quiet",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "quiet"],
                vec!["stash", "apply", "-q"],
            ),
            (
                "quiet-combined",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "quiet"],
                vec!["stash", "apply", "-qq"],
            ),
            (
                "no-quiet",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "quiet"],
                vec!["stash", "apply", "-q", "--no-quiet"],
            ),
            (
                "separator-default",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "separator"],
                vec!["stash", "apply", "--"],
            ),
            (
                "separator-explicit",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "separator"],
                vec!["stash", "apply", "--", "stash@{0}"],
            ),
            (
                "no-index-overrides-index",
                "staged",
                vec!["stash", "push", "-q", "-m", "staged"],
                vec!["stash", "apply", "--index", "--no-index"],
            ),
            (
                "quiet-value",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "quiet"],
                vec!["stash", "apply", "--quiet=false"],
            ),
            (
                "no-quiet-value",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "quiet"],
                vec!["stash", "apply", "--no-quiet=false"],
            ),
            (
                "index-value",
                "staged",
                vec!["stash", "push", "-q", "-m", "staged"],
                vec!["stash", "apply", "--index=false"],
            ),
            (
                "no-index-value",
                "staged",
                vec!["stash", "push", "-q", "-m", "staged"],
                vec!["stash", "apply", "--no-index=false"],
            ),
        ] {
            let template = root.join(format!("{name}-template"));
            prepare_stash_create_repo(&template, setup);
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);
            run_output_with_fixed_identity("git", &upstream, &push_args);
            run_output_with_fixed_identity("git", &actual, &push_args);

            let expected = run_output("git", &upstream, &apply_args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &apply_args);
            assert_eq!(
                actual_output.status.code(),
                expected.status.code(),
                "status differed for stash apply case {name} {apply_args:?}\nactual stdout:\n{}\nactual stderr:\n{}\nexpected stdout:\n{}\nexpected stderr:\n{}",
                String::from_utf8_lossy(&actual_output.stdout),
                String::from_utf8_lossy(&actual_output.stderr),
                String::from_utf8_lossy(&expected.stdout),
                String::from_utf8_lossy(&expected.stderr),
            );
            assert_eq!(
                actual_output.stdout, expected.stdout,
                "stdout differed for stash apply case {name} {apply_args:?}"
            );
            assert_eq!(
                actual_output.stderr, expected.stderr,
                "stderr differed for stash apply case {name} {apply_args:?}"
            );

            for check_args in [
                vec!["status", "--short"],
                vec!["stash", "list", "--format=%gs"],
                vec!["show-ref", "--exists", "refs/stash"],
            ] {
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                assert_same_output(actual_output, expected, &check_args);
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_pop_matches_upstream_git_for_clean_head() {
    let root = unique_temp_dir("stash-pop");
    let result = (|| {
        for (name, setup, push_args, pop_args) in [
            (
                "unstaged",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "unstaged"],
                vec!["stash", "pop"],
            ),
            (
                "staged-index",
                "staged",
                vec!["stash", "push", "-q", "-m", "staged"],
                vec!["stash", "pop", "--index"],
            ),
            (
                "untracked",
                "tracked-and-untracked",
                vec!["stash", "push", "-q", "-u", "-m", "untracked"],
                vec!["stash", "pop"],
            ),
            (
                "quiet",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "quiet"],
                vec!["stash", "pop", "-q"],
            ),
            (
                "quiet-combined",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "quiet"],
                vec!["stash", "pop", "-qq"],
            ),
            (
                "no-quiet",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "quiet"],
                vec!["stash", "pop", "-q", "--no-quiet"],
            ),
            (
                "separator-default",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "separator"],
                vec!["stash", "pop", "--"],
            ),
            (
                "separator-explicit",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "separator"],
                vec!["stash", "pop", "--", "stash@{0}"],
            ),
            (
                "no-index-overrides-index",
                "staged",
                vec!["stash", "push", "-q", "-m", "staged"],
                vec!["stash", "pop", "--index", "--no-index"],
            ),
            (
                "quiet-value",
                "unstaged",
                vec!["stash", "push", "-q", "-m", "quiet"],
                vec!["stash", "pop", "--quiet=false"],
            ),
            (
                "index-value",
                "staged",
                vec!["stash", "push", "-q", "-m", "staged"],
                vec!["stash", "pop", "--index=false"],
            ),
            (
                "no-index-value",
                "staged",
                vec!["stash", "push", "-q", "-m", "staged"],
                vec!["stash", "pop", "--no-index=false"],
            ),
        ] {
            let template = root.join(format!("{name}-template"));
            prepare_stash_create_repo(&template, setup);
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);
            run_output_with_fixed_identity("git", &upstream, &push_args);
            run_output_with_fixed_identity("git", &actual, &push_args);

            let expected = run_output("git", &upstream, &pop_args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &pop_args);
            assert_eq!(
                actual_output.status.code(),
                expected.status.code(),
                "status differed for stash pop case {name} {pop_args:?}\nactual stdout:\n{}\nactual stderr:\n{}\nexpected stdout:\n{}\nexpected stderr:\n{}",
                String::from_utf8_lossy(&actual_output.stdout),
                String::from_utf8_lossy(&actual_output.stderr),
                String::from_utf8_lossy(&expected.stdout),
                String::from_utf8_lossy(&expected.stderr),
            );
            assert_eq!(
                actual_output.stdout, expected.stdout,
                "stdout differed for stash pop case {name} {pop_args:?}"
            );
            assert_eq!(
                actual_output.stderr, expected.stderr,
                "stderr differed for stash pop case {name} {pop_args:?}"
            );

            for check_args in [
                vec!["status", "--short"],
                vec!["stash", "list", "--format=%gs"],
                vec!["show-ref", "--exists", "refs/stash"],
            ] {
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                assert_same_output(actual_output, expected, &check_args);
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_apply_pop_matches_upstream_git_for_moved_head_unchanged_paths() {
    let root = unique_temp_dir("stash-apply-moved-head");
    let result = (|| {
        for (name, setup, apply_args) in [
            ("apply", "unstaged", vec!["stash", "apply"]),
            ("apply-index", "staged", vec!["stash", "apply", "--index"]),
            ("pop", "unstaged", vec!["stash", "pop"]),
            ("pop-index", "staged", vec!["stash", "pop", "--index"]),
        ] {
            let template = root.join(format!("{name}-template"));
            prepare_stash_moved_head_repo(&template, setup);
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            let expected = run_output("git", &upstream, &apply_args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &apply_args);
            assert_same_output(actual_output, expected, &apply_args);

            for check_args in [
                vec!["status", "--short"],
                vec!["stash", "list", "--format=%gs"],
                vec!["show-ref", "--exists", "refs/stash"],
            ] {
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                assert_same_output(actual_output, expected, &check_args);
            }
            assert_eq!(
                fs::read(upstream.join("b.txt")).expect("read upstream moved-head file"),
                fs::read(actual.join("b.txt")).expect("read actual moved-head file"),
                "moved HEAD file differed for stash apply case {name}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn prepare_stash_moved_head_repo(root: &Path, setup: &str) {
    fs::create_dir_all(root).expect("create temp repo");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
    fs::write(root.join("a.txt"), b"base a\n").expect("write first base fixture");
    fs::write(root.join("b.txt"), b"base b\n").expect("write second base fixture");
    git(root, &["add", "a.txt", "b.txt"]);
    git(root, &["commit", "-m", "base", "-q"]);
    fs::write(root.join("a.txt"), b"stashed a\n").expect("write stash fixture");
    if setup == "staged" {
        git(root, &["add", "a.txt"]);
    }
    git(root, &["stash", "push", "-q", "-m", "moved"]);
    fs::write(root.join("b.txt"), b"new head b\n").expect("write moved head fixture");
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "-m", "new head", "-q"]);
}

#[test]
fn stash_apply_pop_empty_and_errors_match_upstream_git() {
    let root = unique_temp_dir("stash-apply-pop-errors");
    let result = (|| {
        for (name, setup, args) in [
            ("apply-empty", "clean", vec!["stash", "apply"]),
            ("pop-empty", "clean", vec!["stash", "pop"]),
            (
                "apply-empty-explicit",
                "clean",
                vec!["stash", "apply", "stash@{99}"],
            ),
            (
                "pop-empty-explicit",
                "clean",
                vec!["stash", "pop", "stash@{99}"],
            ),
            ("apply-empty-bad", "clean", vec!["stash", "apply", "bad"]),
            ("pop-empty-bad", "clean", vec!["stash", "pop", "bad"]),
            (
                "apply-out-of-range",
                "unstaged",
                vec!["stash", "apply", "stash@{99}"],
            ),
            (
                "pop-out-of-range",
                "unstaged",
                vec!["stash", "pop", "stash@{99}"],
            ),
            (
                "apply-too-many",
                "clean",
                vec!["stash", "apply", "stash@{0}", "extra"],
            ),
            (
                "pop-too-many",
                "clean",
                vec!["stash", "pop", "stash@{0}", "extra"],
            ),
            (
                "apply-unknown-option",
                "clean",
                vec!["stash", "apply", "--bogus"],
            ),
            (
                "pop-unknown-option",
                "clean",
                vec!["stash", "pop", "--bogus"],
            ),
            (
                "apply-unknown-switch",
                "clean",
                vec!["stash", "apply", "-q=false"],
            ),
            (
                "apply-combined-quiet-empty",
                "clean",
                vec!["stash", "apply", "-qq"],
            ),
        ] {
            let template = root.join(format!("{name}-template"));
            prepare_stash_create_repo(&template, setup);
            if setup != "clean" {
                run_output_with_fixed_identity("git", &template, &["stash", "push", "-q"]);
            }
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            let expected = run_output("git", &upstream, &args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_branch_matches_upstream_git_for_clean_head() {
    let root = unique_temp_dir("stash-branch");
    let result = (|| {
        for (name, setup, push_args, branch_args) in [
            (
                "unstaged",
                "unstaged",
                vec![vec!["stash", "push", "-q", "-m", "unstaged"]],
                vec!["stash", "branch", "topic-unstaged"],
            ),
            (
                "staged-and-unstaged",
                "staged-and-unstaged",
                vec![vec!["stash", "push", "-q", "-m", "mixed"]],
                vec!["stash", "branch", "topic-mixed"],
            ),
            (
                "untracked",
                "tracked-and-untracked",
                vec![vec!["stash", "push", "-q", "-u", "-m", "untracked"]],
                vec!["stash", "branch", "topic-untracked"],
            ),
            (
                "all",
                "ignored",
                vec![vec!["stash", "push", "-q", "-a", "-m", "all"]],
                vec!["stash", "branch", "topic-all"],
            ),
            (
                "selected",
                "unstaged",
                vec![
                    vec!["stash", "push", "-q", "-m", "first"],
                    vec!["stash", "push", "-q", "-m", "second"],
                ],
                vec!["stash", "branch", "topic-selected", "stash@{1}"],
            ),
        ] {
            let template = root.join(format!("{name}-template"));
            prepare_stash_create_repo(&template, setup);
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);
            for (push_index, args) in push_args.iter().enumerate() {
                if push_index > 0 {
                    fs::write(upstream.join("a.txt"), format!("worktree {push_index}\n"))
                        .expect("write upstream follow-up stash fixture");
                    fs::write(actual.join("a.txt"), format!("worktree {push_index}\n"))
                        .expect("write actual follow-up stash fixture");
                }
                run_output_with_fixed_identity("git", &upstream, args);
                run_output_with_fixed_identity("git", &actual, args);
            }

            let expected = run_output("git", &upstream, &branch_args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &branch_args);
            assert_eq!(
                actual_output.status.code(),
                expected.status.code(),
                "status differed for stash branch case {name} {branch_args:?}\nactual stdout:\n{}\nactual stderr:\n{}\nexpected stdout:\n{}\nexpected stderr:\n{}",
                String::from_utf8_lossy(&actual_output.stdout),
                String::from_utf8_lossy(&actual_output.stderr),
                String::from_utf8_lossy(&expected.stdout),
                String::from_utf8_lossy(&expected.stderr),
            );
            assert_eq!(
                actual_output.stdout, expected.stdout,
                "stdout differed for stash branch case {name} {branch_args:?}"
            );
            assert_eq!(
                actual_output.stderr, expected.stderr,
                "stderr differed for stash branch case {name} {branch_args:?}"
            );

            let branch_name = branch_args[2];
            for check_args in [
                vec!["status", "--short"],
                vec!["stash", "list", "--format=%gs"],
                vec!["branch", "--show-current"],
                vec!["show-ref", "--verify", &format!("refs/heads/{branch_name}")],
            ] {
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output("git", &actual, &check_args);
                assert_same_output(actual_output, expected, &check_args);
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_store_matches_upstream_git() {
    let root = unique_temp_dir("stash-store");
    let template = root.join("template");
    let result = (|| {
        let stash_oid = prepare_stash_store_repo(&template);

        for (name, args) in [
            ("default", vec!["stash", "store", stash_oid.as_str()]),
            (
                "message",
                vec!["stash", "store", "-m", "saved", stash_oid.as_str()],
            ),
            (
                "message-equals",
                vec!["stash", "store", "--message=saved2", stash_oid.as_str()],
            ),
            (
                "message-short",
                vec!["stash", "store", "-msaved3", stash_oid.as_str()],
            ),
            (
                "quiet",
                vec!["stash", "store", "--quiet", stash_oid.as_str()],
            ),
            (
                "no-quiet",
                vec!["stash", "store", "--no-quiet", stash_oid.as_str()],
            ),
        ] {
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            let expected = run_output("git", &upstream, &args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected, &args);

            for check_args in [
                vec!["stash", "list"],
                vec!["show-ref", "--exists", "refs/stash"],
                vec!["rev-parse", "refs/stash"],
            ] {
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                assert_same_output(actual_output, expected, &check_args);
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_store_appends_and_errors_match_upstream_git() {
    let root = unique_temp_dir("stash-store-errors");
    let template = root.join("template");
    let result = (|| {
        let stash_oid = prepare_stash_store_repo(&template);
        let second_stash_oid =
            String::from_utf8(git(&template, &["stash", "create", "second fixture"]))
                .expect("second stash create oid is utf8")
                .trim()
                .to_string();
        let head_oid = String::from_utf8(git(&template, &["rev-parse", "HEAD"]))
            .expect("head oid is utf8")
            .trim()
            .to_string();
        let blob_oid = String::from_utf8(git(&template, &["hash-object", "-w", "a.txt"]))
            .expect("blob oid is utf8")
            .trim()
            .to_string();

        for (name, args) in [
            (
                "append",
                vec!["stash", "store", "-m", "first", second_stash_oid.as_str()],
            ),
            (
                "same-oid-noop",
                vec!["stash", "store", "-m", "first", stash_oid.as_str()],
            ),
            ("missing", vec!["stash", "store"]),
            ("bad-revision", vec!["stash", "store", "bad"]),
            (
                "too-many",
                vec!["stash", "store", stash_oid.as_str(), "extra"],
            ),
            ("missing-message", vec!["stash", "store", "-m"]),
            ("missing-message-long", vec!["stash", "store", "--message"]),
            (
                "not-stash-commit",
                vec!["stash", "store", head_oid.as_str()],
            ),
            (
                "blob",
                vec!["stash", "store", "-m", "blob", blob_oid.as_str()],
            ),
        ] {
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            if matches!(name, "append" | "same-oid-noop") {
                git(&upstream, &["stash", "store", stash_oid.as_str()]);
                git_rs(&actual, &["stash", "store", stash_oid.as_str()]);
            }

            let expected = run_output("git", &upstream, &args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected, &args);

            if matches!(name, "append" | "same-oid-noop") {
                let check_args = ["stash", "list"];
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                assert_same_output(actual_output, expected, &check_args);
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_list_empty_matches_upstream_git() {
    let root = unique_temp_dir("stash-list-empty");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        for args in [
            vec!["stash", "list"],
            vec!["stash", "list", "--oneline"],
            vec!["stash", "list", "--format=%gd"],
            vec!["stash", "list", "--format=%gs"],
            vec!["stash", "list", "-1"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(
                actual, expected,
                "git-rs empty stash output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_clear_matches_upstream_git() {
    let root = unique_temp_dir("stash-clear");
    let template = root.join("template");
    let upstream = root.join("upstream");
    let actual = root.join("actual");
    let result = (|| {
        prepare_stash_repo(&template);
        copy_dir(&template, &upstream);
        copy_dir(&template, &actual);

        let args = ["stash", "clear"];
        let expected = run_output("git", &upstream, &args);
        let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
        assert_same_output(actual_output, expected, &args);

        for args in [
            vec!["stash", "list"],
            vec!["status", "--show-stash"],
            vec!["show-ref", "--exists", "refs/stash"],
        ] {
            let expected = run_output("git", &upstream, &args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_clear_empty_and_errors_match_upstream_git() {
    let root = unique_temp_dir("stash-clear-empty-errors");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);

        for args in [
            vec!["stash", "clear"],
            vec!["stash", "clear", "extra"],
            vec!["stash", "clear", "--bogus"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_drop_matches_upstream_git() {
    let root = unique_temp_dir("stash-drop");
    let template = root.join("template");
    let result = (|| {
        prepare_stash_repo(&template);

        for (name, args) in [
            ("default", vec!["stash", "drop"]),
            ("explicit", vec!["stash", "drop", "stash@{1}"]),
            ("full-ref", vec!["stash", "drop", "refs/stash@{0}"]),
            ("quiet", vec!["stash", "drop", "--quiet", "stash@{0}"]),
            ("no-quiet", vec!["stash", "drop", "--no-quiet", "stash@{0}"]),
        ] {
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            let expected = run_output("git", &upstream, &args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected, &args);

            for check_args in [
                vec!["stash", "list"],
                vec!["status", "--show-stash"],
                vec!["show-ref", "--exists", "refs/stash"],
            ] {
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                assert_same_output(actual_output, expected, &check_args);
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_drop_last_and_errors_match_upstream_git() {
    let root = unique_temp_dir("stash-drop-last-errors");
    let template = root.join("template");
    let result = (|| {
        prepare_single_stash_repo(&template);

        for (name, args) in [
            ("last", vec!["stash", "drop"]),
            ("empty", vec!["stash", "drop", "stash@{99}"]),
            ("invalid", vec!["stash", "drop", "bad"]),
            ("unknown-option", vec!["stash", "drop", "--bogus"]),
            ("too-many", vec!["stash", "drop", "stash@{0}", "extra"]),
        ] {
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            let expected = run_output("git", &upstream, &args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected, &args);

            if name == "last" {
                for check_args in [
                    vec!["stash", "list"],
                    vec!["status", "--show-stash"],
                    vec!["show-ref", "--exists", "refs/stash"],
                ] {
                    let expected = run_output("git", &upstream, &check_args);
                    let actual_output =
                        run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                    assert_same_output(actual_output, expected, &check_args);
                }
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_show_matches_upstream_git() {
    let root = unique_temp_dir("stash-show");
    let result = (|| {
        prepare_stash_repo(&root);
        for args in [
            vec!["stash", "show"],
            vec!["stash", "show", "--stat"],
            vec!["stash", "show", "-s"],
            vec!["stash", "show", "--no-patch"],
            vec!["stash", "show", "-s", "--stat"],
            vec!["stash", "show", "--stat", "-s"],
            vec!["stash", "show", "-s", "-p"],
            vec!["stash", "show", "-p", "-s"],
            vec!["stash", "show", "--name-only"],
            vec!["stash", "show", "--name-status"],
            vec!["stash", "show", "--name-status", "--stat"],
            vec!["stash", "show", "--stat", "--name-status"],
            vec!["stash", "show", "--name-status", "-p"],
            vec!["stash", "show", "-p", "--name-status"],
            vec!["stash", "show", "--name-only", "--stat"],
            vec!["stash", "show", "--stat", "--name-only"],
            vec!["stash", "show", "--numstat"],
            vec!["stash", "show", "--shortstat"],
            vec!["stash", "show", "--summary"],
            vec!["stash", "show", "--compact-summary"],
            vec!["stash", "show", "--diff-filter=M"],
            vec!["stash", "show", "--diff-filter=A"],
            vec!["stash", "show", "--diff-filter=m"],
            vec!["stash", "show", "--diff-filter=M*"],
            vec!["stash", "show", "--diff-filter=A*"],
            vec!["stash", "show", "--raw"],
            vec!["stash", "show", "--raw", "--abbrev=12"],
            vec!["stash", "show", "--raw", "--abbrev"],
            vec!["stash", "show", "--raw", "--no-abbrev"],
            vec!["stash", "show", "--stat", "--numstat"],
            vec!["stash", "show", "--numstat", "--stat"],
            vec!["stash", "show", "--shortstat", "--numstat"],
            vec!["stash", "show", "--numstat", "--shortstat"],
            vec!["stash", "show", "--raw", "--numstat"],
            vec!["stash", "show", "--numstat", "--raw"],
            vec!["stash", "show", "--raw", "--stat"],
            vec!["stash", "show", "--stat", "--raw"],
            vec!["stash", "show", "--compact-summary", "--summary"],
            vec!["stash", "show", "--summary", "--compact-summary"],
            vec!["stash", "show", "--name-only", "--diff-filter=M"],
            vec!["stash", "show", "--name-status", "--diff-filter=M"],
            vec!["stash", "show", "--raw", "--diff-filter=M"],
            vec!["stash", "show", "--numstat", "--diff-filter=M"],
            vec!["stash", "show", "-p", "--diff-filter=M"],
            vec!["stash", "show", "--raw", "-p"],
            vec!["stash", "show", "-p", "--raw"],
            vec!["stash", "show", "--patch-with-raw"],
            vec!["stash", "show", "--patch-with-stat"],
            vec!["stash", "show", "--stat", "-p"],
            vec!["stash", "show", "-p", "--stat"],
            vec!["stash", "show", "-p", "--abbrev=12"],
            vec!["stash", "show", "-p", "--full-index"],
            vec!["stash", "show", "--no-full-index"],
            vec!["stash", "show", "-p"],
            vec!["stash", "show", "--patch"],
            vec!["stash", "show", "--oneline"],
            vec!["stash", "show", "--no-compact-summary"],
            vec!["stash", "show", "--quiet", "--no-quiet"],
            vec!["stash", "show", "--name-only", "--quiet", "--no-quiet"],
            vec!["stash", "show", "--quiet", "--no-quiet", "--name-only"],
            vec!["stash", "show", "--exit-code", "--no-exit-code"],
            vec!["stash", "show", "--ext-diff"],
            vec!["stash", "show", "--no-ext-diff"],
            vec!["stash", "show", "--textconv"],
            vec!["stash", "show", "--no-textconv"],
            vec!["stash", "show", "stash@{1}"],
            vec!["stash", "show", "refs/stash@{0}"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(
                actual, expected,
                "git-rs stash show output differed for {args:?}"
            );
        }
        let args = ["stash", "show", "--quiet"];
        let expected = run_output("git", &root, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        let args = ["stash", "show", "--exit-code"];
        let expected = run_output("git", &root, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        for args in [
            ["stash", "show", "--quiet=false"].as_slice(),
            ["stash", "show", "--no-quiet=false"].as_slice(),
            ["stash", "show", "--exit-code=false"].as_slice(),
            ["stash", "show", "--no-exit-code=false"].as_slice(),
            ["stash", "show", "--stat=false"].as_slice(),
            ["stash", "show", "--no-stat"].as_slice(),
            ["stash", "show", "--no-stat=false"].as_slice(),
            ["stash", "show", "--raw=false"].as_slice(),
            ["stash", "show", "--no-raw"].as_slice(),
            ["stash", "show", "--no-raw=false"].as_slice(),
            ["stash", "show", "--name-only=false"].as_slice(),
            ["stash", "show", "--no-name-only"].as_slice(),
            ["stash", "show", "--name-status=false"].as_slice(),
            ["stash", "show", "--no-name-status"].as_slice(),
            ["stash", "show", "--numstat=false"].as_slice(),
            ["stash", "show", "--no-numstat"].as_slice(),
            ["stash", "show", "--shortstat=false"].as_slice(),
            ["stash", "show", "--no-shortstat"].as_slice(),
            ["stash", "show", "--summary=false"].as_slice(),
            ["stash", "show", "--no-summary"].as_slice(),
            ["stash", "show", "--compact-summary=false"].as_slice(),
            ["stash", "show", "--no-compact-summary=false"].as_slice(),
            ["stash", "show", "--patch=false"].as_slice(),
            ["stash", "show", "--no-patch=false"].as_slice(),
            ["stash", "show", "--full-index=false"].as_slice(),
            ["stash", "show", "--no-full-index=false"].as_slice(),
            ["stash", "show", "--ext-diff=false"].as_slice(),
            ["stash", "show", "--no-ext-diff=false"].as_slice(),
            ["stash", "show", "--textconv=false"].as_slice(),
            ["stash", "show", "--no-textconv=false"].as_slice(),
        ] {
            let expected = run_output("git", &root, args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, args);
            assert_same_output(actual, expected, args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_show_untracked_flags_match_upstream_git() {
    let root = unique_temp_dir("stash-show-untracked");
    let untracked = root.join("untracked");
    let tracked_only = root.join("tracked-only");
    let result = (|| {
        prepare_untracked_stash_repo(&untracked);
        for args in [
            vec!["stash", "show", "--include-untracked"],
            vec!["stash", "show", "-u"],
            vec!["stash", "show", "--only-untracked"],
            vec!["stash", "show", "--name-only", "--include-untracked"],
            vec!["stash", "show", "--name-only", "--only-untracked"],
            vec!["stash", "show", "--name-status", "--include-untracked"],
            vec!["stash", "show", "--name-status", "--only-untracked"],
            vec!["stash", "show", "--numstat", "--include-untracked"],
            vec!["stash", "show", "--numstat", "--only-untracked"],
            vec!["stash", "show", "--shortstat", "--include-untracked"],
            vec!["stash", "show", "--shortstat", "--only-untracked"],
            vec!["stash", "show", "--summary", "--include-untracked"],
            vec!["stash", "show", "--summary", "--only-untracked"],
            vec!["stash", "show", "--compact-summary", "--include-untracked"],
            vec!["stash", "show", "--compact-summary", "--only-untracked"],
            vec!["stash", "show", "--diff-filter=A", "--include-untracked"],
            vec!["stash", "show", "--diff-filter=M", "--include-untracked"],
            vec!["stash", "show", "--diff-filter=A", "--only-untracked"],
            vec!["stash", "show", "--diff-filter=M", "--only-untracked"],
            vec![
                "stash",
                "show",
                "--only-untracked",
                "--no-include-untracked",
            ],
            vec![
                "stash",
                "show",
                "--include-untracked",
                "--no-include-untracked",
            ],
            vec![
                "stash",
                "show",
                "--name-status",
                "--diff-filter=A",
                "--include-untracked",
            ],
            vec!["stash", "show", "--raw", "--include-untracked"],
            vec!["stash", "show", "--raw", "--only-untracked"],
            vec!["stash", "show", "--patch-with-raw", "--include-untracked"],
            vec![
                "stash",
                "show",
                "--compact-summary",
                "-p",
                "--include-untracked",
            ],
            vec!["stash", "show", "--summary", "-p", "--include-untracked"],
            vec!["stash", "show", "--name-only", "--no-include-untracked"],
            vec![
                "stash",
                "show",
                "--name-only",
                "--include-untracked",
                "--no-include-untracked",
            ],
            vec![
                "stash",
                "show",
                "--name-only",
                "--only-untracked",
                "--include-untracked",
            ],
            vec![
                "stash",
                "show",
                "--name-only",
                "--include-untracked",
                "--only-untracked",
            ],
            vec!["stash", "show", "--stat", "--include-untracked"],
            vec!["stash", "show", "--stat", "--only-untracked"],
            vec!["stash", "show", "-p", "--include-untracked"],
            vec!["stash", "show", "-p", "--only-untracked"],
        ] {
            let expected = git(&untracked, &args);
            let actual = git_rs(&untracked, &args);
            assert_eq!(
                actual, expected,
                "git-rs stash show untracked output differed for {args:?}"
            );
        }
        for args in [
            ["stash", "show", "--quiet", "--include-untracked"].as_slice(),
            ["stash", "show", "--quiet", "--only-untracked"].as_slice(),
            ["stash", "show", "--quiet", "--numstat"].as_slice(),
            ["stash", "show", "--numstat", "--quiet"].as_slice(),
            ["stash", "show", "--quiet", "--raw"].as_slice(),
            ["stash", "show", "--raw", "--quiet"].as_slice(),
            ["stash", "show", "--quiet", "--compact-summary"].as_slice(),
            ["stash", "show", "--compact-summary", "--quiet"].as_slice(),
            ["stash", "show", "--quiet", "--diff-filter=A"].as_slice(),
            ["stash", "show", "--quiet", "--diff-filter=M"].as_slice(),
            ["stash", "show", "--quiet", "-s"].as_slice(),
            ["stash", "show", "-s", "--quiet"].as_slice(),
            ["stash", "show", "--quiet", "--name-status"].as_slice(),
            ["stash", "show", "--name-status", "--quiet"].as_slice(),
            ["stash", "show", "-s", "--name-only"].as_slice(),
            ["stash", "show", "-s", "--name-status"].as_slice(),
            ["stash", "show", "--name-only", "-s"].as_slice(),
            ["stash", "show", "--name-only", "--name-status"].as_slice(),
            ["stash", "show", "--name-status", "--name-only"].as_slice(),
            ["stash", "show", "--no-only-untracked"].as_slice(),
            ["stash", "show", "--include-untracked=false"].as_slice(),
            ["stash", "show", "--no-include-untracked=false"].as_slice(),
            ["stash", "show", "--only-untracked=false"].as_slice(),
            ["stash", "show", "--no-only-untracked=false"].as_slice(),
            [
                "stash",
                "show",
                "--include-untracked",
                "--only-untracked",
                "--no-only-untracked",
            ]
            .as_slice(),
        ] {
            let expected = run_output("git", &untracked, args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &untracked, args);
            assert_same_output(actual, expected, args);
        }

        prepare_tracked_only_stash_repo(&tracked_only);
        for args in [
            vec!["stash", "show", "--only-untracked"],
            vec!["stash", "show", "--name-only", "--only-untracked"],
        ] {
            let expected = git(&tracked_only, &args);
            let actual = git_rs(&tracked_only, &args);
            assert_eq!(
                actual, expected,
                "git-rs tracked-only stash output differed for {args:?}"
            );
        }
        let args = ["stash", "show", "--quiet", "--only-untracked"];
        let expected = run_output("git", &tracked_only, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &tracked_only, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_show_empty_and_errors_match_upstream_git() {
    let root = unique_temp_dir("stash-show-empty-errors");
    let template = root.join("template");
    let result = (|| {
        prepare_single_stash_repo(&template);

        for (name, args) in [
            ("empty", vec!["stash", "show", "stash@{99}"]),
            ("invalid", vec!["stash", "show", "bad"]),
            ("too-many", vec!["stash", "show", "stash@{0}", "extra"]),
            ("bad-diff-filter", vec!["stash", "show", "--diff-filter=Z"]),
            (
                "missing-diff-filter",
                vec!["stash", "show", "--diff-filter"],
            ),
            (
                "bad-diff-filter-separate",
                vec!["stash", "show", "--diff-filter", "Z"],
            ),
        ] {
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            let expected = run_output("git", &upstream, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual, expected, &args);
        }

        let empty = root.join("empty-repo");
        fs::create_dir_all(&empty).expect("create empty repo");
        git(&empty, &["init", "-q"]);
        let args = ["stash", "show"];
        let expected = run_output("git", &empty, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &empty, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
