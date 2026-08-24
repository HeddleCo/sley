//! Oracle-comparison coverage for the approxidate port
//! (`sley_core::date::approxidate`), exercised through its observable CLI
//! surface: `git config --type=expiry-date --default=<value>` canonicalises an
//! arbitrary date string through git's `parse_expiry_date` →
//! `approxidate_careful` and prints the resulting Unix timestamp.
//!
//! Absolute forms must match byte-for-byte. Relative forms (`2.weeks.ago`,
//! `yesterday`, …) resolve against "now", so oracle and sley invocations may
//! straddle a wall-clock second boundary; those assert agreement within one
//! second instead of exact bytes.
//!
//! Known pre-existing delta (documented, not pinned here): for values that do
//! NOT canonicalise, oracle prints an extra `error: '<v>' for '<key>' is not a
//! valid timestamp` line before dying, and sley's config diagnostic layer does
//! not reproduce that wording yet. The exit codes (128) already agree.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn init_repo(name: &str) -> PathBuf {
    let root = unique_temp_dir(name);
    std::fs::create_dir_all(&root).expect("create temp dir");
    let status = Command::new(sley_testkit::oracle_git())
        .arg("init")
        .arg("-q")
        .arg(&root)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init failed");
    root
}

/// Run `config --type=expiry-date --default=<value> --get <missing-key>` under
/// `TZ=UTC` (the upstream test harness convention the port's UTC civil-math
/// convention is calibrated against).
fn expiry_date_output(program: &str, cwd: &Path, value: &str) -> Output {
    sley_testkit::hermetic_git_command(program)
        .current_dir(cwd)
        .env("TZ", "UTC")
        .args([
            "config",
            "--type=expiry-date",
            "--default",
            value,
            "--get",
            "sley.test.missingkey",
        ])
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} for {value:?}: {err}"))
}

#[test]
fn absolute_forms_match_oracle_byte_for_byte() {
    let repo = init_repo("approxidate-absolute");
    for value in [
        "@1700000000 +0000",
        "@1234567890 -0530",
        "2005-04-07T22:13:13",
        "1970-01-01T00:00:01Z",
        "2017/11/11 11:11:11PM",
        "2017/11/10 09:08:07 PM",
        "Fri Jun 4 15:46:55 2010",
        "2001-09-17",
    ] {
        let expected = expiry_date_output(sley_testkit::oracle_git(), &repo, value);
        let actual = expiry_date_output(sley_testkit::sley_bin!(), &repo, value);
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "exit code differed for {value:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "stdout differed for {value:?}"
        );
    }
}

#[test]
fn relative_forms_track_now_within_one_second() {
    let repo = init_repo("approxidate-relative");
    for value in [
        "2.weeks.ago",
        "yesterday",
        "noon",
        "tea",
        "last week GMT",
        "now.yesterday",
    ] {
        let expected = expiry_date_output(sley_testkit::oracle_git(), &repo, value);
        let actual = expiry_date_output(sley_testkit::sley_bin!(), &repo, value);
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "exit code differed for {value:?}"
        );
        let expected_ts: u64 = String::from_utf8_lossy(&expected.stdout)
            .trim()
            .parse()
            .unwrap_or_else(|err| panic!("oracle output for {value:?} not a timestamp: {err}"));
        let actual_ts: u64 = String::from_utf8_lossy(&actual.stdout)
            .trim()
            .parse()
            .unwrap_or_else(|err| panic!("sley output for {value:?} not a timestamp: {err}"));
        let drift = expected_ts.abs_diff(actual_ts);
        assert!(
            drift <= 1,
            "sley drifted {drift}s from oracle for {value:?} ({actual_ts} vs {expected_ts})"
        );
    }
}

#[test]
fn sentinels_and_rejections_match_exit_codes() {
    let repo = init_repo("approxidate-sentinels");
    for value in [
        "never",
        "false",
        "all", // sentinels: exact stdout too
        "abc",
        "True",
        "~/dir",
        ":(optional)no-such-path",
        "", // rejections
    ] {
        let expected = expiry_date_output(sley_testkit::oracle_git(), &repo, value);
        let actual = expiry_date_output(sley_testkit::sley_bin!(), &repo, value);
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "exit code differed for {value:?}\noracle stderr:\n{}\nsley stderr:\n{}",
            String::from_utf8_lossy(&expected.stderr),
            String::from_utf8_lossy(&actual.stderr),
        );
        if value == "never" || value == "false" || value == "all" {
            assert_eq!(
                actual.stdout, expected.stdout,
                "stdout differed for {value:?}"
            );
        }
    }
}
