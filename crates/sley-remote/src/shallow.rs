//! Reading and updating the repository's `$GIT_DIR/shallow` boundary file.
//!
//! A shallow repository records the object ids of the commits at the edge of its
//! truncated history in `$GIT_DIR/shallow`: one full hex oid per line, sorted as
//! git writes it (lexicographically by hex, which equals binary oid order). The
//! file is absent for a complete (non-shallow) repository.
//!
//! [`read_shallow`] loads the boundary the client must replay as `shallow` lines
//! in a deepen request; [`apply_shallow_info`] folds the server's shallow-info
//! response ([`ProtocolV2FetchShallowInfo`]) back into the file after the pack is
//! installed — adding `shallow` entries and dropping `unshallow` ones — and
//! removes the file when the repository becomes complete, matching git.

use std::fs;
use std::path::Path;

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_protocol::ProtocolV2FetchShallowInfo;

/// The boundary commit ids recorded in `$GIT_DIR/shallow`, or an empty vec when
/// the file is absent (a complete repository). Lines are parsed as full hex oids
/// of `format`; blank lines are ignored.
pub fn read_shallow(git_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let path = git_dir.join("shallow");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut oids = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        oids.push(ObjectId::from_hex(format, line).map_err(|err| {
            GitError::InvalidFormat(format!("invalid oid in {}: {err}", path.display()))
        })?);
    }
    Ok(oids)
}

/// Write `$GIT_DIR/shallow` from `oids`, sorting and de-duplicating them the way
/// git does (by hex, one per line, trailing newline). An empty set removes the
/// file so the repository reads as complete.
pub fn write_shallow(git_dir: &Path, oids: &[ObjectId]) -> Result<()> {
    let path = git_dir.join("shallow");
    let mut hexes = oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>();
    hexes.sort();
    hexes.dedup();
    if hexes.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        return Ok(());
    }
    let mut contents = String::with_capacity(hexes.iter().map(|hex| hex.len() + 1).sum());
    for hex in &hexes {
        contents.push_str(hex);
        contents.push('\n');
    }
    fs::write(&path, contents)?;
    Ok(())
}

/// Fold the server's shallow-info `entries` into `$GIT_DIR/shallow`: the new
/// boundary is the existing set plus every `shallow <oid>` minus every
/// `unshallow <oid>`. A no-op when `entries` is empty (so a deepen request that
/// reported no boundary change leaves the file untouched). Mirrors git's update
/// of the shallow file after a deepen fetch.
pub fn apply_shallow_info(
    git_dir: &Path,
    format: ObjectFormat,
    entries: &[ProtocolV2FetchShallowInfo],
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut oids = read_shallow(git_dir, format)?;
    for entry in entries {
        match entry {
            ProtocolV2FetchShallowInfo::Shallow(oid) => {
                if !oids.contains(oid) {
                    oids.push(oid.clone());
                }
            }
            ProtocolV2FetchShallowInfo::Unshallow(oid) => oids.retain(|existing| existing != oid),
        }
    }
    write_shallow(git_dir, &oids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sley-remote-shallow-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn oid(hex_byte: &str) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, &hex_byte.repeat(40)).expect("valid oid")
    }

    #[test]
    fn read_missing_shallow_is_empty() {
        let dir = temp_dir();
        assert!(read_shallow(&dir, ObjectFormat::Sha1).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_sorts_dedups_and_round_trips() {
        let dir = temp_dir();
        let a = oid("1");
        let b = oid("2");
        write_shallow(&dir, &[b.clone(), a.clone(), b.clone()]).unwrap();
        let contents = fs::read_to_string(dir.join("shallow")).unwrap();
        assert_eq!(contents, format!("{}\n{}\n", a.to_hex(), b.to_hex()));
        assert_eq!(read_shallow(&dir, ObjectFormat::Sha1).unwrap(), vec![a, b]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_empty_removes_file() {
        let dir = temp_dir();
        write_shallow(&dir, &[oid("3")]).unwrap();
        assert!(dir.join("shallow").exists());
        write_shallow(&dir, &[]).unwrap();
        assert!(!dir.join("shallow").exists());
        // Removing an already-absent file is fine.
        write_shallow(&dir, &[]).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_adds_shallow_and_drops_unshallow() {
        let dir = temp_dir();
        let keep = oid("a");
        let added = oid("b");
        let removed = oid("c");
        write_shallow(&dir, &[keep.clone(), removed.clone()]).unwrap();
        apply_shallow_info(
            &dir,
            ObjectFormat::Sha1,
            &[
                ProtocolV2FetchShallowInfo::Shallow(added.clone()),
                ProtocolV2FetchShallowInfo::Unshallow(removed),
            ],
        )
        .unwrap();
        // The file is written sorted by hex, so `read_shallow` returns
        // `keep` (aaaa…) before `added` (bbbb…), with `removed` (cccc…) gone.
        assert_eq!(
            read_shallow(&dir, ObjectFormat::Sha1).unwrap(),
            vec![keep, added]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_empty_entries_is_noop() {
        let dir = temp_dir();
        let existing = oid("d");
        write_shallow(&dir, std::slice::from_ref(&existing)).unwrap();
        apply_shallow_info(&dir, ObjectFormat::Sha1, &[]).unwrap();
        assert_eq!(
            read_shallow(&dir, ObjectFormat::Sha1).unwrap(),
            vec![existing]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_unshallowing_last_boundary_removes_file() {
        let dir = temp_dir();
        let boundary = oid("e");
        write_shallow(&dir, std::slice::from_ref(&boundary)).unwrap();
        apply_shallow_info(
            &dir,
            ObjectFormat::Sha1,
            &[ProtocolV2FetchShallowInfo::Unshallow(boundary)],
        )
        .unwrap();
        assert!(!dir.join("shallow").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
