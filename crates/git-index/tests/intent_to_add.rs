//! Intent-to-add (`git add -N`) support: constructing/reading ITA entries, and
//! reproducing git's own ITA index byte-for-byte.

use std::path::Path;
use std::process::Command;

use git_core::{ObjectFormat, ObjectId};
use git_index::{Index, IndexEntry, Stage, INDEX_EXTENDED_FLAG_INTENT_TO_ADD, INDEX_FLAG_EXTENDED};

fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn intent_to_add_entry_round_trips_from_scratch() {
    let format = ObjectFormat::Sha1;
    let mut index = Index {
        version: 2,
        entries: vec![IndexEntry::intent_to_add(format, b"new.txt".to_vec())],
        extensions: Vec::new(),
        checksum: None,
    };
    // An extended entry cannot live in a v2 index; this raises it to v3.
    index.upgrade_version_for_flags();
    assert_eq!(index.version, 3);

    let entry = &index.entries[0];
    assert!(entry.is_intent_to_add());
    assert!(!entry.is_skip_worktree());
    assert_eq!(entry.stage(), Stage::Normal);
    assert_eq!(entry.oid, ObjectId::empty_blob(format));
    assert_eq!(entry.flags & INDEX_FLAG_EXTENDED, INDEX_FLAG_EXTENDED);

    let bytes = index.write(format).unwrap();
    let parsed = Index::parse(&bytes, format).unwrap();
    assert_eq!(parsed.version, index.version);
    assert_eq!(parsed.entries, index.entries);
    assert_eq!(parsed.extensions, index.extensions);
    assert!(parsed.entries[0].is_intent_to_add());
}

#[test]
fn set_and_clear_intent_to_add_tracks_extended_bit() {
    let format = ObjectFormat::Sha1;
    let mut entry = IndexEntry::intent_to_add(format, b"f".to_vec());
    assert!(entry.is_intent_to_add());

    entry.set_intent_to_add(false);
    assert!(!entry.is_intent_to_add());
    // No other extended bits set, so the extended flag is cleared too.
    assert_eq!(entry.flags & INDEX_FLAG_EXTENDED, 0);
    assert_eq!(entry.flags_extended & INDEX_EXTENDED_FLAG_INTENT_TO_ADD, 0);

    entry.set_intent_to_add(true);
    assert!(entry.is_intent_to_add());
    assert_eq!(entry.flags & INDEX_FLAG_EXTENDED, INDEX_FLAG_EXTENDED);
}

#[test]
fn parses_and_round_trips_git_add_dash_n_index_byte_for_byte() {
    let tmp = std::env::temp_dir().join(format!("git-rs-ita-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    if !git_ok(&tmp, &["--version"]) {
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }

    assert!(git_ok(&tmp, &["init", "-q"]));
    std::fs::write(tmp.join("tracked.txt"), b"already\n").unwrap();
    assert!(git_ok(&tmp, &["add", "tracked.txt"]));
    std::fs::write(tmp.join("pending.txt"), b"pending\n").unwrap();
    // `-N` records an intent-to-add placeholder for an otherwise-untracked file.
    assert!(git_ok(&tmp, &["add", "-N", "pending.txt"]));

    let index_path = tmp.join(".git").join("index");
    let original = std::fs::read(&index_path).unwrap();
    let format = ObjectFormat::Sha1;
    let index = Index::parse(&original, format).unwrap();

    // git bumped the index to v3 to carry the extended (ITA) entry.
    assert!(index.version >= 3);
    let ita: Vec<&IndexEntry> = index
        .entries
        .iter()
        .filter(|entry| entry.is_intent_to_add())
        .collect();
    assert_eq!(ita.len(), 1);
    assert_eq!(ita[0].path, b"pending.txt");
    assert_eq!(ita[0].oid, ObjectId::empty_blob(format));
    // The pre-existing tracked entry is a normal, non-ITA entry.
    assert!(index
        .entries
        .iter()
        .any(|entry| entry.path == b"tracked.txt" && !entry.is_intent_to_add()));

    // Re-serializing git's index must reproduce it byte-for-byte.
    let rewritten = index.write(format).unwrap();
    assert_eq!(
        rewritten, original,
        "intent-to-add index must round-trip byte-for-byte"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
