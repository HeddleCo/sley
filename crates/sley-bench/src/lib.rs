//! # sley-bench
//!
//! Criterion benchmarks for `cat-file`, object-database read paths, and graph/ref/tree walks.
//!
//! Run full benchmarks:
//!
//! ```text
//! cargo bench -p sley-bench
//! ```
//!
//! Quick compile/smoke run (fewer samples):
//!
//! ```text
//! cargo bench -p sley-bench --bench cat_file -- --quick
//! ```

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_formats::{CommitGraph, CommitGraphWriteEntry};
use sley_object::BString;
use sley_object::{Commit, EncodedObject, ObjectType, Tree, TreeEntry};
use sley_odb::{FileObjectDatabase, ObjectWriter};
use sley_pack::{PackFile, PackWrite, PackWriteOptions};
use sley_refs::{FileRefStore, RefTarget, RefUpdate};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Number of deltifiable blob objects written into the benchmark pack fixture.
pub const FIXTURE_OBJECT_COUNT: usize = 500;

/// Larger pack fixture used by prefix-resolution benchmarks.
pub const LARGE_FIXTURE_OBJECT_COUNT: usize = 1000;

/// Number of commits written into the commit-graph benchmark fixture.
pub const COMMIT_FIXTURE_COUNT: usize = 200;

/// Number of branch refs created in the commit-graph benchmark fixture.
pub const BRANCH_REF_COUNT: usize = 10;

#[derive(Debug)]
pub struct BenchFixture {
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub format: ObjectFormat,
    pub object_ids: Vec<ObjectId>,
    pub sample_oid: ObjectId,
}

impl BenchFixture {
    pub fn database(&self) -> FileObjectDatabase {
        FileObjectDatabase::from_git_dir(&self.git_dir, self.format)
    }

    pub fn batch_input(&self, count: usize) -> Vec<u8> {
        let count = count.min(self.object_ids.len());
        let mut input = Vec::with_capacity(count * (self.format.hex_len() + 1));
        for oid in &self.object_ids[..count] {
            input.extend_from_slice(oid.to_hex().as_bytes());
            input.push(b'\n');
        }
        input
    }
}

/// Build the deltified blob pack used by the pack and cat-file benchmarks.
pub fn build_blob_pack() -> Result<PackWrite> {
    build_blob_pack_with_count(FIXTURE_OBJECT_COUNT)
}

/// Build a deltified blob pack with `count` entries.
pub fn build_blob_pack_with_count(count: usize) -> Result<PackWrite> {
    let format = ObjectFormat::Sha1;
    let objects = (0..count)
        .map(|index| EncodedObject::new(ObjectType::Blob, deltifiable_blob_body(index)))
        .collect::<Vec<_>>();
    let options = PackWriteOptions::new().with_window(50);
    let written = PackFile::write_packed_with_options(&objects, format, &options)?;
    if written.entries.len() != count {
        return Err(GitError::InvalidFormat(format!(
            "expected {count} pack entries, got {}",
            written.entries.len()
        )));
    }
    Ok(written)
}

#[derive(Debug)]
pub struct PackInstallTarget {
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub format: ObjectFormat,
}

/// Create an empty repository suitable for repeated `install_pack` measurements.
pub fn create_pack_install_target() -> Result<PackInstallTarget> {
    let format = ObjectFormat::Sha1;
    let repo_root = unique_temp_dir("sley-bench-pack-install");
    let git_dir = repo_root.join(".git");
    init_minimal_repo(&git_dir)?;
    Ok(PackInstallTarget {
        repo_root,
        git_dir,
        format,
    })
}

/// Build a temporary git repository containing one pack with
/// [`FIXTURE_OBJECT_COUNT`] deltified blobs.
pub fn create_fixture() -> Result<BenchFixture> {
    create_fixture_with_count(FIXTURE_OBJECT_COUNT)
}

/// Build a temporary git repository containing one pack with `count` deltified blobs.
pub fn create_fixture_with_count(count: usize) -> Result<BenchFixture> {
    let format = ObjectFormat::Sha1;
    let repo_root = unique_temp_dir("sley-bench-fixture");
    let git_dir = repo_root.join(".git");
    init_minimal_repo(&git_dir)?;

    let written = build_blob_pack_with_count(count)?;
    let object_ids = written
        .entries
        .iter()
        .map(|entry| entry.oid.clone())
        .collect::<Vec<_>>();
    let sample_oid = object_ids[count / 2].clone();

    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    db.install_pack(&written)?;

    // Ensure objects are only available from the installed pack.
    for oid in &object_ids {
        let loose_path = db.loose().object_path(oid)?;
        if loose_path.exists() {
            fs::remove_file(loose_path)?;
        }
    }

    Ok(BenchFixture {
        repo_root,
        git_dir,
        format,
        object_ids,
        sample_oid,
    })
}

#[derive(Debug)]
pub struct CommitBenchFixture {
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub format: ObjectFormat,
    pub head_oid: ObjectId,
    pub commit_count: usize,
    pub branch_refs: Vec<String>,
}

/// Build a temporary git repository with a linear history, commit-graph, HEAD,
/// and [`BRANCH_REF_COUNT`] branch refs.
pub fn create_commit_fixture() -> Result<CommitBenchFixture> {
    let format = ObjectFormat::Sha1;
    let repo_root = unique_temp_dir("sley-bench-commit-fixture");
    let git_dir = repo_root.join(".git");
    init_commit_repo(&git_dir)?;

    let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let refs = FileRefStore::new(&git_dir, format);

    let mut parent: Option<ObjectId> = None;
    let mut commit_oids = Vec::with_capacity(COMMIT_FIXTURE_COUNT);
    let mut graph_entries = Vec::with_capacity(COMMIT_FIXTURE_COUNT);

    for index in 0..COMMIT_FIXTURE_COUNT {
        let tree = write_commit_tree(&mut db, index)?;
        let parents = parent.iter().cloned().collect::<Vec<_>>();
        let commit_time = 1_000_000 + index as u64;
        let identity = format!("Benchmark User <bench@example.invalid> {commit_time} +0000");
        let commit = Commit {
            tree: tree.clone(),
            parents,
            author: identity.as_bytes().to_vec(),
            committer: identity.into_bytes(),
            encoding: None,
            message: format!("commit {index}\n").into_bytes(),
        };
        let oid = db.write_object(EncodedObject::new(ObjectType::Commit, commit.write()))?;
        graph_entries.push(CommitGraphWriteEntry {
            oid: oid.clone(),
            tree,
            parents: parent.iter().cloned().collect(),
            generation: (index + 1) as u32,
            commit_time,
            bloom_filter: None,
        });
        parent = Some(oid.clone());
        commit_oids.push(oid);
    }

    let head_oid = commit_oids
        .last()
        .cloned()
        .ok_or_else(|| GitError::InvalidFormat("commit fixture produced no commits".into()))?;

    fs::create_dir_all(git_dir.join("objects").join("info"))?;
    fs::write(
        git_dir.join("objects").join("info").join("commit-graph"),
        CommitGraph::write(format, &graph_entries)?,
    )?;

    let mut branch_refs = Vec::with_capacity(BRANCH_REF_COUNT);
    for branch_index in 0..BRANCH_REF_COUNT {
        let commit_index = if branch_index == 0 {
            COMMIT_FIXTURE_COUNT - 1
        } else {
            (branch_index * (COMMIT_FIXTURE_COUNT - 1)) / (BRANCH_REF_COUNT - 1)
        };
        let branch = if branch_index == 0 {
            "main".to_string()
        } else {
            format!("branch-{branch_index}")
        };
        let name = format!("refs/heads/{branch}");
        branch_refs.push(name.clone());
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name,
            expected: None,
            new: RefTarget::Direct(commit_oids[commit_index].clone()),
            reflog: None,
        });
        tx.commit()?;
    }

    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".to_string(),
        expected: None,
        new: RefTarget::Symbolic("refs/heads/main".to_string()),
        reflog: None,
    });
    tx.commit()?;

    Ok(CommitBenchFixture {
        repo_root,
        git_dir,
        format,
        head_oid,
        commit_count: COMMIT_FIXTURE_COUNT,
        branch_refs,
    })
}

fn init_commit_repo(git_dir: &Path) -> Result<()> {
    fs::create_dir_all(git_dir.join("objects").join("pack"))?;
    fs::create_dir_all(git_dir.join("refs").join("heads"))?;
    fs::create_dir_all(git_dir.join("refs").join("tags"))?;
    Ok(())
}

fn write_commit_tree(db: &mut FileObjectDatabase, index: usize) -> Result<ObjectId> {
    let root_blob = db.write_object(EncodedObject::new(
        ObjectType::Blob,
        format!("root payload {index}\n").into_bytes(),
    ))?;
    let nested_blob = db.write_object(EncodedObject::new(
        ObjectType::Blob,
        format!("nested payload {index}\n").into_bytes(),
    ))?;
    let deep_blob = db.write_object(EncodedObject::new(
        ObjectType::Blob,
        format!("deep payload {index}\n").into_bytes(),
    ))?;

    let nested_tree = db.write_object(EncodedObject::new(
        ObjectType::Tree,
        Tree {
            entries: vec![TreeEntry {
                mode: 0o100644,
                name: BString::from(b"leaf.txt"),
                oid: deep_blob,
            }],
        }
        .write(),
    ))?;
    let subdir_tree = db.write_object(EncodedObject::new(
        ObjectType::Tree,
        Tree {
            entries: vec![
                TreeEntry {
                    mode: 0o100644,
                    name: BString::from(b"nested.txt"),
                    oid: nested_blob,
                },
                TreeEntry {
                    mode: 0o040000,
                    name: BString::from(b"nested"),
                    oid: nested_tree,
                },
            ],
        }
        .write(),
    ))?;
    db.write_object(EncodedObject::new(
        ObjectType::Tree,
        Tree {
            entries: vec![
                TreeEntry {
                    mode: 0o100644,
                    name: BString::from(format!("file-{index}.txt").into_bytes()),
                    oid: root_blob,
                },
                TreeEntry {
                    mode: 0o040000,
                    name: BString::from(b"subdir"),
                    oid: subdir_tree,
                },
            ],
        }
        .write(),
    ))
}

fn init_minimal_repo(git_dir: &Path) -> Result<()> {
    fs::create_dir_all(git_dir.join("objects").join("pack"))?;
    fs::create_dir_all(git_dir.join("refs").join("heads"))?;
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")?;
    Ok(())
}

fn deltifiable_blob_body(index: usize) -> Vec<u8> {
    let mut body = Vec::new();
    for _ in 0..400 {
        body.extend_from_slice(b"common payload line for delta compression\n");
    }
    body.extend_from_slice(format!("variant-{index:04}\n").as_bytes());
    body
}

/// Run the release `sley` binary built by `build.rs`.
pub fn run_sley(cwd: &Path, args: &[&str], stdin: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child =
        Command::new(std::env::var("SLEY_BENCH_BIN").map_err(|_| {
            GitError::Command("SLEY_BENCH_BIN not set; build sley-bench first".into())
        })?)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !stdin.is_empty() {
        let stdin_handle = child
            .stdin
            .as_mut()
            .ok_or_else(|| GitError::Command("missing sley stdin".into()))?;
        stdin_handle
            .write_all(stdin)
            .map_err(|err| GitError::Io(err.to_string()))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(format!(
            "sley {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout)
}

/// Run the system `git` binary for head-to-head CLI comparisons.
pub fn run_git(cwd: &Path, args: &[&str], stdin: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !stdin.is_empty() {
        let stdin_handle = child
            .stdin
            .as_mut()
            .ok_or_else(|| GitError::Command("missing git stdin".into()))?;
        stdin_handle
            .write_all(stdin)
            .map_err(|err| GitError::Io(err.to_string()))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout)
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{nanos}-{}",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}
