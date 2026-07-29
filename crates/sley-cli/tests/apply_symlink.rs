//! Symlink-escape guards for `git apply` (CVE-2023-23946 class / t4115 / t4122).
//!
//! These cases exercise `check_apply_path_safety` without requiring the full
//! upstream t-suite: create/rename under a newly introduced symlink must be
//! refused, while legitimate typechanges (delete symlink then create files
//! under a real directory) must still apply.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
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

fn sley(cwd: &Path, args: &[&str]) -> Output {
    Command::new(sley_testkit::sley_bin!())
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap_or_else(|err| panic!("failed to run sley {args:?}: {err}"))
}

fn sley_ok(cwd: &Path, args: &[&str]) {
    let out = sley(cwd, args);
    assert!(
        out.status.success(),
        "sley {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write_patch(repo: &Path, name: &str, body: &str) -> PathBuf {
    let path = repo.join(name);
    fs::write(&path, body).expect("write patch");
    path
}

/// t4115 #5: rename an existing symlink, then create a file under the new
/// name — must refuse with "beyond a symbolic link" and not write outside.
#[test]
fn apply_refuses_create_under_renamed_symlink() {
    let root = unique_temp_dir("apply-symlink-create");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("test");
    sley_ok(&repo, &["init"]);
    symlink(".git", repo.join("symlink")).expect("symlink");
    sley_ok(&repo, &["add", "symlink"]);
    sley_ok(
        &repo,
        &[
            "-c",
            "user.name=Tester",
            "-c",
            "user.email=tester@example.com",
            "commit",
            "-m",
            "add symlink",
        ],
    );

    let patch = write_patch(
        &repo,
        "escape.patch",
        "\
diff --git a/symlink b/renamed-symlink
similarity index 100%
rename from symlink
rename to renamed-symlink
--
diff --git /dev/null b/renamed-symlink/create-me
new file mode 100644
index 0000000..039727e
--- /dev/null
+++ b/renamed-symlink/create-me
@@ -0,0 +1,1 @@
+busted
",
    );

    let out = sley(&repo, &["apply", patch.to_str().expect("test")]);
    assert!(
        !out.status.success(),
        "apply should fail for create under renamed symlink"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("beyond a symbolic link"), "stderr={stderr}");
    assert!(
        !repo.join(".git/create-me").exists(),
        "must not write through symlink into .git"
    );
    // Atomic guard: the rename must not have been materialised either.
    assert!(
        repo.join("symlink").exists(),
        "failed apply must leave the original symlink in place"
    );
    assert!(
        !repo.join("renamed-symlink").exists(),
        "failed apply must not leave a partial rename"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Explicit `new mode 120000` symlink create + rename/copy deposit under it
/// must be refused (the hole left by gating created_symlinks on `is_new` only).
#[test]
fn apply_refuses_rename_under_explicit_created_symlink() {
    let root = unique_temp_dir("apply-symlink-rename-under");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("test");
    sley_ok(&repo, &["init"]);
    fs::write(repo.join("victim"), "payload\n").expect("test");
    sley_ok(&repo, &["add", "victim"]);
    sley_ok(
        &repo,
        &[
            "-c",
            "user.name=Tester",
            "-c",
            "user.email=tester@example.com",
            "commit",
            "-m",
            "victim",
        ],
    );

    // Outside target the symlink would point at; a successful escape would
    // place `escape-me` here.
    let outside = root.join("outside");
    fs::create_dir_all(&outside).expect("test");

    let patch = write_patch(
        &repo,
        "escape.patch",
        "\
diff --git a/tmp b/tmp
new file mode 120000
index 0000000..1111111
--- /dev/null
+++ b/tmp
@@ -0,0 +1 @@
+../outside
\\ No newline at end of file
diff --git a/victim b/tmp/escape-me
similarity index 100%
rename from victim
rename to tmp/escape-me
",
    );

    let out = sley(&repo, &["apply", patch.to_str().expect("test")]);
    assert!(
        !out.status.success(),
        "apply should refuse rename under a just-created symlink"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("beyond a symbolic link"), "stderr={stderr}");
    assert!(
        !outside.join("escape-me").exists(),
        "must not write through the new symlink into outside/"
    );
    assert!(
        repo.join("victim").exists(),
        "failed apply must leave the rename source intact"
    );

    let _ = fs::remove_dir_all(&root);
}

/// t4122-style typechange: delete a symlink, then create a real directory
/// tree at the same path — must succeed.
#[test]
fn apply_allows_symlink_to_directory_typechange() {
    let root = unique_temp_dir("apply-symlink-typechange");
    let repo = root.join("repo");
    fs::create_dir_all(repo.join("arch/i386/boot")).expect("test");
    fs::create_dir_all(repo.join("arch/x86_64")).expect("test");
    fs::write(repo.join("arch/i386/boot/Makefile"), "1\n2\n3\n").expect("test");
    symlink("../i386/boot", repo.join("arch/x86_64/boot")).expect("symlink");
    sley_ok(&repo, &["init"]);
    sley_ok(&repo, &["add", "."]);
    sley_ok(
        &repo,
        &[
            "-c",
            "user.name=Tester",
            "-c",
            "user.email=tester@example.com",
            "commit",
            "-m",
            "initial",
        ],
    );

    let patch = write_patch(
        &repo,
        "typechange.patch",
        "\
diff --git a/arch/x86_64/boot b/arch/x86_64/boot
deleted file mode 120000
index ad3f146..0000000
--- a/arch/x86_64/boot
+++ /dev/null
@@ -1 +0,0 @@
-../i386/boot
\\ No newline at end of file
diff --git a/arch/x86_64/boot/Makefile b/arch/x86_64/boot/Makefile
new file mode 100644
index 0000000..33e5156
--- /dev/null
+++ b/arch/x86_64/boot/Makefile
@@ -0,0 +1,3 @@
+2
+3
+4
",
    );

    let out = sley(&repo, &["apply", patch.to_str().expect("test")]);
    assert!(
        out.status.success(),
        "typechange apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let makefile = repo.join("arch/x86_64/boot/Makefile");
    assert!(makefile.is_file(), "expected real file at typechanged path");
    assert_eq!(fs::read_to_string(makefile).expect("test"), "2\n3\n4\n");
    // The old symlink must be gone (replaced by a directory).
    let meta = fs::symlink_metadata(repo.join("arch/x86_64/boot")).expect("test");
    assert!(meta.is_dir());

    let _ = fs::remove_dir_all(&root);
}

/// Existing worktree symlink: creating a file under it is refused.
#[test]
fn apply_refuses_create_under_existing_symlink() {
    let root = unique_temp_dir("apply-symlink-existing");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("test");
    sley_ok(&repo, &["init"]);
    symlink(".git", repo.join("link")).expect("symlink");
    // Do not stage the link; plain worktree apply still sees it via lstat.

    let patch = write_patch(
        &repo,
        "escape.patch",
        "\
diff --git /dev/null b/link/create-me
new file mode 100644
index 0000000..039727e
--- /dev/null
+++ b/link/create-me
@@ -0,0 +1,1 @@
+busted
",
    );

    let out = sley(&repo, &["apply", patch.to_str().expect("test")]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("beyond a symbolic link"), "stderr={stderr}");
    assert!(!repo.join(".git/create-me").exists());

    let _ = fs::remove_dir_all(&root);
}
