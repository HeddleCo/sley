//! Oracle comparisons against the reference `git` binary for the sley-mail
//! text engines: `git mailsplit` vs [`sley_mail::mailinfo::split_mbox`] /
//! mboxrd unescaping, `git mailinfo` header decoding vs the MIME path, and
//! `git patch-id --stable` vs the patch-id hash core.
//!
//! Runs are hermetic (`GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` pinned to
//! /dev/null); each test skips (no failure) when no `git` binary is available
//! so the crate's unit tests remain standalone.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Output};

use sley_core::{ObjectFormat, ObjectId};
use sley_mail::mailinfo::{
    SubjectCleanup, parse_message, split_keep_newline, split_mbox, unescape_mboxrd,
};
use sley_mail::patch_id::{PatchIdOptions, get_one_patchid};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .is_ok()
}

fn git(args: &[&str], stdin: &[u8], cwd: Option<&std::path::Path>) -> Output {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    use std::io::Write;
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(stdin)
        .expect("write stdin");
    child.wait_with_output().expect("wait for git")
}

/// A crafted two-message mbox: format-patch style separators, an encoded-word
/// subject, folded headers, and a `>From ` body line that only the mboxrd path
/// unescapes.
const MBOX: &str = concat!(
    "From 1111111111111111111111111111111111111111 Mon Sep 17 00:00:00 2001\n",
    "From: A U Thor <author@example.com>\n",
    "Date: Sun, 27 Sep 2026 11:06:40 +0200\n",
    "Subject: [PATCH 1/2] =?UTF-8?q?first=20subject?= \n",
    " with continuation\n",
    "Message-ID: <first@example.com>\n",
    "\n",
    "body of first\n",
    ">From escaped line\n",
    "---\n",
    " diff --git a/f b/f\n",
    "--- a/f\n",
    "+++ b/f\n",
    "@@ -1 +1 @@\n",
    "-a\n",
    "+b\n",
    "-- \n",
    "2.55.0\n",
    "\n",
    "From 2222222222222222222222222222222222222222 Mon Sep 17 00:00:00 2001\n",
    "From: =?ISO-8859-1?q?Ni=EFve=20Author?= <naive@example.com>\n",
    "Date: Mon, 28 Sep 2026 01:02:03 -0500\n",
    "Subject: [PATCH 2/2] second subject\n",
    "\n",
    "body of second\n",
    "---\n",
    " diff --git a/g b/g\n",
    "--- a/g\n",
    "+++ b/g\n",
    "@@ -1 +1 @@\n",
    "-c\n",
    "+d\n",
    "-- \n",
    "2.55.0\n",
);

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sley-mail-oracle-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

#[test]
fn mailsplit_matches_oracle() {
    if !git_available() {
        eprintln!("skipping: no git binary");
        return;
    }
    let input = MBOX.as_bytes();

    // Plain mbox: compare our raw splitter against the files `git mailsplit`
    // writes (numbered, separator line dropped, content verbatim).
    let dir = tempdir("mailsplit");
    let out_arg = format!("-o{}", dir.display());
    let out = git(&["mailsplit", &out_arg], input, None);
    assert!(
        out.status.success(),
        "git mailsplit failed: {:?}",
        out.stderr
    );
    let mut expected: Vec<Vec<u8>> = Vec::new();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("list mailsplit output")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in &names {
        let raw = std::fs::read(dir.join(name)).expect("read mailsplit file");
        // `git mailsplit` keeps the mbox "From " separator as the first line of
        // each output file; drop it so the comparison covers message content.
        let first_nl = raw
            .iter()
            .position(|&b| b == b'\n')
            .expect("mailsplit file has a separator line");
        expected.push(raw[first_nl + 1..].to_vec());
    }
    assert_eq!(names.len(), 2, "expected two messages from mailsplit");

    // Plain mbox split: byte-for-byte against the oracle files.
    let got = split_mbox(input);
    assert_eq!(got.len(), expected.len());
    for (ours, theirs) in got.iter().zip(expected.iter()) {
        assert_eq!(ours, theirs, "raw split mismatch vs git mailsplit");
    }

    // Sanity: the oracle keeps mboxrd `>From ` escaping in place (`git
    // mailsplit` does not unescape); our --patch-format=mboxrd path does.
    assert!(expected[0].windows(6).any(|w| w == b">From "));
    let unescaped = unescape_mboxrd(&got[0]);
    assert!(!unescaped.windows(6).any(|w| w == b">From "));
    assert!(unescaped.windows(17).any(|w| w == b"From escaped line"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mailinfo_header_decode_matrix_matches_oracle() {
    if !git_available() {
        eprintln!("skipping: no git binary");
        return;
    }

    // (label, Date header, From header value, Subject header value)
    let cases: Vec<(&str, &str, &str, &str)> = vec![
        (
            "plain",
            "Sun, 27 Sep 2026 11:06:40 +0200",
            "A U Thor <author@example.com>",
            "[PATCH] plain subject",
        ),
        (
            "q-encoded-utf8-subject",
            "Sun, 27 Sep 2026 11:06:40 +0200",
            "A U Thor <author@example.com>",
            "=?UTF-8?q?[PATCH]_caf=C3=A9_subject?=",
        ),
        (
            "b-encoded-utf8-subject",
            "Mon, 28 Sep 2026 01:02:03 -0500",
            "A U Thor <author@example.com>",
            "=?UTF-8?B?W1BBVENIXSBjYWYgwqkgc3ViamVjdA==?=",
        ),
        (
            "latin1-display-name",
            "Tue, 29 Sep 2026 23:59:60 +0000",
            "=?ISO-8859-1?q?Ni=EFve=20Author?= <naive@example.com>",
            "[PATCH] ascii subject",
        ),
        (
            "folded-from-and-subject",
            "Thu Dec 4 16:00:00 2008 -0800",
            "Folded Name\n <folded@example.com>",
            "=?UTF-8?q?[PATCH]_multi?=\n =?UTF-8?q?word_subject?=",
        ),
    ];

    for (label, date, from, subject) in cases {
        let mail = format!(
            "From: {from}\nDate: {date}\nSubject: {subject}\nMessage-ID: <{label}@example.com>\n\nbody text\n---\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n"
        );
        let out = git(
            &["mailinfo", "/dev/null", "/dev/null"],
            mail.as_bytes(),
            None,
        );
        assert!(out.status.success(), "git mailinfo failed for {label}");
        let oracle = String::from_utf8_lossy(&out.stdout).into_owned();

        let mut oracle_fields: BTreeMap<&str, String> = BTreeMap::new();
        for line in oracle.lines() {
            for key in ["Author", "Email", "Subject", "Date"] {
                if let Some(rest) = line.strip_prefix(&format!("{key}: ")) {
                    oracle_fields.insert(key, rest.to_string());
                }
            }
        }

        let lines = split_keep_newline(mail.as_bytes());
        let msg = parse_message(&lines, SubjectCleanup::default()).expect("parse failed");

        assert_eq!(
            String::from_utf8_lossy(&msg.author_name),
            oracle_fields["Author"],
            "author mismatch for {label}"
        );
        assert_eq!(
            String::from_utf8_lossy(&msg.author_email),
            oracle_fields["Email"],
            "email mismatch for {label}"
        );
        assert_eq!(
            msg.subject,
            oracle_fields["Subject"].trim_end(),
            "subject mismatch for {label}"
        );
        // Our stored raw date keeps the original header text, like the oracle.
        assert_eq!(
            msg.author_date_raw.as_deref(),
            Some(oracle_fields["Date"].as_str()),
            "raw date mismatch for {label}"
        );
    }
}

#[test]
fn patch_id_stable_matches_oracle() {
    if !git_available() {
        eprintln!("skipping: no git binary");
        return;
    }

    let diff = concat!(
        "diff --git a/foo b/foo\n",
        "index 1234567..89abcde 100644\n",
        "--- a/foo\n",
        "+++ b/foo\n",
        "@@ -1,3 +1,3 @@\n",
        " context line\n",
        "-old removed line\n",
        "+new added line\n",
        " more context\n",
        "diff --git a/bar b/bar\n",
        "index 0000000..1111111 100644\n",
        "--- a/bar\n",
        "+++ b/bar\n",
        "@@ -0,0 +1 @@\n",
        "+brand new file\n",
    );

    // Run outside any repository (hermetic): git defaults to SHA-1 there.
    let tmp = tempdir("patch-id");
    let out = git(&["patch-id", "--stable"], diff.as_bytes(), Some(&tmp));
    assert!(
        out.status.success(),
        "git patch-id failed: {:?}",
        out.stderr
    );
    let oracle_line = String::from_utf8_lossy(&out.stdout);
    let oracle_id = oracle_line.split(' ').next().expect("patch-id output");
    assert!(!oracle_id.is_empty());

    let options = PatchIdOptions {
        stable: true,
        verbatim: false,
    };
    let lines = sley_mail::patch_id::split_keep_newlines(diff.as_bytes());
    let mut cursor = 0usize;
    let patch = get_one_patchid(&lines, &mut cursor, ObjectFormat::Sha1, &options);
    let oid = ObjectId::from_raw(ObjectFormat::Sha1, &patch.result).expect("oid");
    assert_eq!(
        oid.to_hex(),
        oracle_id,
        "patch-id --stable digest differs from oracle"
    );

    // Unstable mode must also agree (same single-patch input).
    let out = git(&["patch-id", "--unstable"], diff.as_bytes(), Some(&tmp));
    assert!(out.status.success());
    let oracle_line = String::from_utf8_lossy(&out.stdout);
    let oracle_id = oracle_line.split(' ').next().expect("patch-id output");
    let options = PatchIdOptions {
        stable: false,
        verbatim: false,
    };
    let mut cursor = 0usize;
    let patch = get_one_patchid(&lines, &mut cursor, ObjectFormat::Sha1, &options);
    let oid = ObjectId::from_raw(ObjectFormat::Sha1, &patch.result).expect("oid");
    assert_eq!(oid.to_hex(), oracle_id);

    let _ = std::fs::remove_dir_all(&tmp);
}
