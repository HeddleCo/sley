//! Compare-and-swap preconditions on the file-backed ref transaction:
//! create-only (`MustNotExist`), match-or-create (`ExistingMustMatch`),
//! `MustExist`, batch atomicity, and back-compat with `RefUpdate::expected`.

use std::path::{Path, PathBuf};

use sley_core::{ObjectFormat, ObjectId};
use sley_refs::{FileRefStore, RefPrecondition, RefTarget, RefUpdate};

fn unique_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sley-cas-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn store(dir: &Path) -> FileRefStore {
    FileRefStore::new(dir.to_path_buf(), ObjectFormat::Sha1)
}

fn oid(nibble: char) -> ObjectId {
    ObjectId::from_hex(ObjectFormat::Sha1, &String::from(nibble).repeat(40)).unwrap()
}

#[test]
fn must_not_exist_is_create_only() {
    let dir = unique_dir("mne");
    let s = store(&dir);
    let a = oid('1');

    let mut tx = s.transaction();
    tx.update_to(
        "refs/heads/x",
        RefTarget::Direct(a.clone()),
        RefPrecondition::MustNotExist,
        None,
    );
    tx.commit().unwrap();
    assert_eq!(
        s.read_ref("refs/heads/x").unwrap(),
        Some(RefTarget::Direct(a.clone()))
    );

    // A second create must fail and leave the value intact.
    let mut tx = s.transaction();
    tx.update_to(
        "refs/heads/x",
        RefTarget::Direct(oid('2')),
        RefPrecondition::MustNotExist,
        None,
    );
    assert!(tx.commit().is_err());
    assert_eq!(
        s.read_ref("refs/heads/x").unwrap(),
        Some(RefTarget::Direct(a))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn existing_must_match_allows_absent_or_equal() {
    let dir = unique_dir("emm");
    let s = store(&dir);
    let a = oid('a');

    // Absent -> allowed (creates).
    let mut tx = s.transaction();
    tx.update_to(
        "refs/heads/y",
        RefTarget::Direct(a.clone()),
        RefPrecondition::ExistingMustMatch(RefTarget::Direct(a.clone())),
        None,
    );
    tx.commit().unwrap();

    // Present and matching -> allowed (updates).
    let b = oid('b');
    let mut tx = s.transaction();
    tx.update_to(
        "refs/heads/y",
        RefTarget::Direct(b.clone()),
        RefPrecondition::ExistingMustMatch(RefTarget::Direct(a.clone())),
        None,
    );
    tx.commit().unwrap();
    assert_eq!(
        s.read_ref("refs/heads/y").unwrap(),
        Some(RefTarget::Direct(b.clone()))
    );

    // Present but differing -> rejected.
    let mut tx = s.transaction();
    tx.update_to(
        "refs/heads/y",
        RefTarget::Direct(oid('c')),
        RefPrecondition::ExistingMustMatch(RefTarget::Direct(a)),
        None,
    );
    assert!(tx.commit().is_err());
    assert_eq!(
        s.read_ref("refs/heads/y").unwrap(),
        Some(RefTarget::Direct(b))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn must_exist_requires_presence() {
    let dir = unique_dir("me");
    let s = store(&dir);

    let mut tx = s.transaction();
    tx.update_to(
        "refs/heads/z",
        RefTarget::Direct(oid('1')),
        RefPrecondition::MustExist,
        None,
    );
    assert!(tx.commit().is_err());
    assert_eq!(s.read_ref("refs/heads/z").unwrap(), None);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn failed_precondition_rolls_back_whole_batch() {
    let dir = unique_dir("atomic");
    let s = store(&dir);
    let a = oid('1');

    let mut tx = s.transaction();
    tx.update_to(
        "refs/heads/keep",
        RefTarget::Direct(a.clone()),
        RefPrecondition::MustNotExist,
        None,
    );
    tx.commit().unwrap();

    // One valid create plus one that violates MustNotExist on an existing ref:
    // the whole commit must fail and neither change may land.
    let mut tx = s.transaction();
    tx.update_to(
        "refs/heads/new",
        RefTarget::Direct(oid('2')),
        RefPrecondition::MustNotExist,
        None,
    );
    tx.update_to(
        "refs/heads/keep",
        RefTarget::Direct(oid('3')),
        RefPrecondition::MustNotExist,
        None,
    );
    assert!(tx.commit().is_err());
    assert_eq!(
        s.read_ref("refs/heads/new").unwrap(),
        None,
        "the new ref must not have been created"
    );
    assert_eq!(
        s.read_ref("refs/heads/keep").unwrap(),
        Some(RefTarget::Direct(a)),
        "the existing ref must be unchanged"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn refupdate_expected_still_behaves_as_must_match() {
    let dir = unique_dir("compat");
    let s = store(&dir);
    let a = oid('1');

    let mut tx = s.transaction();
    tx.update(RefUpdate {
        name: "refs/heads/c".into(),
        expected: None,
        new: RefTarget::Direct(a.clone()),
        reflog: None,
    });
    tx.commit().unwrap();

    // expected = Some(matching) -> ok.
    let b = oid('2');
    let mut tx = s.transaction();
    tx.update(RefUpdate {
        name: "refs/heads/c".into(),
        expected: Some(RefTarget::Direct(a.clone())),
        new: RefTarget::Direct(b.clone()),
        reflog: None,
    });
    tx.commit().unwrap();

    // expected = Some(wrong) -> rejected, value unchanged.
    let mut tx = s.transaction();
    tx.update(RefUpdate {
        name: "refs/heads/c".into(),
        expected: Some(RefTarget::Direct(a)),
        new: RefTarget::Direct(oid('9')),
        reflog: None,
    });
    assert!(tx.commit().is_err());
    assert_eq!(
        s.read_ref("refs/heads/c").unwrap(),
        Some(RefTarget::Direct(b))
    );

    let _ = std::fs::remove_dir_all(&dir);
}
