use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_formats::{Bundle, BundleReference};
use sley_index::Index;
use sley_object::{EncodedObject, ObjectType, TreeEntries, tree_entry_object_type};
use sley_odb::{FileObjectDatabase, LooseObjectStore, ObjectReader, ObjectWriter};
use sley_pack::{PackFile, PackIndex, PackWriteOptions};
use sley_refs::{FileRefStore, PackedRef, Ref, RefTarget, RefUpdate, ReflogEntry};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The major.minor of git that sley's parity tests are written against.
///
/// Oracle output (the upstream `git` we diff sley against) is version-sensitive:
/// flags, default formatting, and error wording change across releases, so a
/// mismatched oracle produces large numbers of spurious parity "failures". The
/// guard in [`oracle_git`] pins the oracle to this series.
const REQUIRED_ORACLE_GIT_SERIES: &str = "2.54";

static ORACLE_GIT: OnceLock<&'static str> = OnceLock::new();

#[cfg(not(windows))]
pub const HERMETIC_GIT_CONFIG_PATH: &str = "/dev/null";
#[cfg(windows)]
pub const HERMETIC_GIT_CONFIG_PATH: &str = "NUL";

/// Stable author/committer name used by test subprocesses that need identity.
pub const TEST_GIT_USER_NAME: &str = "Example User";
/// Stable author/committer email used by test subprocesses that need identity.
pub const TEST_GIT_USER_EMAIL: &str = "example@example.invalid";
/// Stable author/committer date used by test subprocesses that need identity.
pub const TEST_GIT_IDENT_DATE: &str = "@0 +0000";

const HERMETIC_GIT_ENV_REMOVE: &[&str] = &["GIT_CONFIG", "GIT_CONFIG_PARAMETERS"];

/// Return a `Command` for an oracle-git-like program with host config sealed
/// off.
///
/// The command starts with `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` pointed
/// at [`HERMETIC_GIT_CONFIG_PATH`] and `GIT_CONFIG_COUNT=0`, so ordinary test
/// invocations do not inherit a developer's global/system config or ambient
/// `GIT_CONFIG_COUNT` injection. Callers may still deliberately override these
/// variables after construction when a test is specifically exercising config
/// injection.
pub fn hermetic_git_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    apply_hermetic_git_env(&mut command);
    command
}

/// Return a hermetic git command with deterministic author/committer identity.
///
/// Use this for tests that create commits or otherwise ask git for author or
/// committer identity. Tests that only read repository state should prefer
/// [`hermetic_git_command`].
pub fn hermetic_git_command_with_identity(program: impl AsRef<OsStr>) -> Command {
    let mut command = hermetic_git_command(program);
    apply_standard_git_identity_env(&mut command);
    command
}

/// Apply sley's hermetic git environment to an existing command.
pub fn apply_hermetic_git_env(command: &mut Command) -> &mut Command {
    for name in HERMETIC_GIT_ENV_REMOVE {
        command.env_remove(name);
    }
    command
        .env("GIT_CONFIG_GLOBAL", HERMETIC_GIT_CONFIG_PATH)
        .env("GIT_CONFIG_SYSTEM", HERMETIC_GIT_CONFIG_PATH)
        .env("GIT_CONFIG_COUNT", "0")
}

/// Apply sley's deterministic author/committer identity to an existing command.
pub fn apply_standard_git_identity_env(command: &mut Command) -> &mut Command {
    command
        .env("GIT_AUTHOR_NAME", TEST_GIT_USER_NAME)
        .env("GIT_AUTHOR_EMAIL", TEST_GIT_USER_EMAIL)
        .env("GIT_AUTHOR_DATE", TEST_GIT_IDENT_DATE)
        .env("GIT_COMMITTER_NAME", TEST_GIT_USER_NAME)
        .env("GIT_COMMITTER_EMAIL", TEST_GIT_USER_EMAIL)
        .env("GIT_COMMITTER_DATE", TEST_GIT_IDENT_DATE)
}

/// Returns the program name/path to use as the **oracle git** in parity tests.
///
/// Resolution order:
/// * `$SLEY_TEST_GIT`, if set — the explicit, pinnable override (point it at a
///   `git-2.54/bin/git`).
/// * otherwise `"git"` (the PATH git).
///
/// On first call this also runs a one-time version guard (see
/// [`assert_oracle_git_version`]): if the resolved oracle is not on the
/// [`REQUIRED_ORACLE_GIT_SERIES`] series it panics with an actionable message,
/// converting the silent-skew failure mode (comparing against the wrong git and
/// getting dozens of bogus diffs) into a single, self-explaining error.
///
/// The returned value is `&'static str` so it is a drop-in for the `program`
/// argument of the per-test `run`/`run_output`/… helpers. New helpers should
/// create subprocesses through [`hermetic_git_command`] rather than raw
/// `Command::new(...)`.
pub fn oracle_git() -> &'static str {
    ORACLE_GIT.get_or_init(|| {
        // Leak once: the oracle program is fixed for the lifetime of the test
        // process, and callers want a `&'static str`.
        let program: &'static str = match std::env::var("SLEY_TEST_GIT") {
            Ok(path) if !path.is_empty() => Box::leak(path.into_boxed_str()),
            _ => "git",
        };
        assert_oracle_git_version(program);
        program
    })
}

/// Runs `<program> --version`, parses the reported version, and panics unless it
/// is on the [`REQUIRED_ORACLE_GIT_SERIES`] series.
fn assert_oracle_git_version(program: &str) {
    let output = hermetic_git_command(program)
        .arg("--version")
        .output()
        .unwrap_or_else(|err| {
            panic!(
                "sley parity tests could not run the oracle git `{program} --version`: {err}. \
                 Install git {REQUIRED_ORACLE_GIT_SERIES} and put it on PATH, or set \
                 SLEY_TEST_GIT=/path/to/git-{REQUIRED_ORACLE_GIT_SERIES}/bin/git."
            )
        });
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported = stdout.trim();
    let version = parse_git_version(reported).unwrap_or_else(|| {
        panic!(
            "sley parity tests could not parse the oracle git version from `{program} --version` \
             (got {reported:?}). Expected git {REQUIRED_ORACLE_GIT_SERIES}.x."
        )
    });
    if !version_on_series(&version, REQUIRED_ORACLE_GIT_SERIES) {
        panic!(
            "sley parity tests require git {REQUIRED_ORACLE_GIT_SERIES}.x as the oracle; \
             found {version} (via `{program}`). Install git {REQUIRED_ORACLE_GIT_SERIES} and put \
             it on PATH, or set SLEY_TEST_GIT=/path/to/git-{REQUIRED_ORACLE_GIT_SERIES}/bin/git."
        );
    }
}

/// Extracts the dotted version (e.g. `2.54.0`) from a `git --version` line such
/// as `git version 2.54.0`. Returns `None` if no version token is found.
fn parse_git_version(version_line: &str) -> Option<String> {
    version_line
        .split_whitespace()
        .find(|token| token.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|token| token.to_string())
}

/// True if `version` (e.g. `2.54.0` or `2.54.1.windows.1`) is on the
/// `series` series (e.g. `2.54`), i.e. its `major.minor` prefix matches.
fn version_on_series(version: &str, series: &str) -> bool {
    let mut v = version.split('.');
    let mut s = series.split('.');
    matches!(
        (v.next(), s.next(), v.next(), s.next()),
        (Some(vmaj), Some(smaj), Some(vmin), Some(smin)) if vmaj == smaj && vmin == smin
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashObjectCase {
    pub object_type: String,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityResult {
    pub case_name: String,
    pub upstream: String,
    pub rust: String,
}

pub fn hash_object_parity(cases: &[HashObjectCase]) -> Result<Vec<ParityResult>> {
    hash_object_parity_for_format(ObjectFormat::Sha1, cases)
}

pub fn hash_object_parity_for_format(
    format: ObjectFormat,
    cases: &[HashObjectCase],
) -> Result<Vec<ParityResult>> {
    let mut results = Vec::with_capacity(cases.len());
    for (idx, case) in cases.iter().enumerate() {
        let rust = sley_core::object_id_for_bytes(format, &case.object_type, &case.body)?.to_hex();
        let upstream = upstream_git_hash_object(format, &case.object_type, &case.body)?;
        if rust != upstream {
            return Err(GitError::Command(format!(
                "{} hash-object mismatch for case {idx}: rust {rust}, upstream {upstream}",
                format.name()
            )));
        }
        results.push(ParityResult {
            case_name: format!("hash-object-{}-{idx}", format.name()),
            upstream,
            rust,
        });
    }
    Ok(results)
}

pub fn default_hash_object_cases() -> Vec<HashObjectCase> {
    vec![
        HashObjectCase {
            object_type: "blob".into(),
            body: Vec::new(),
        },
        HashObjectCase {
            object_type: "blob".into(),
            body: b"hello\n".to_vec(),
        },
        HashObjectCase {
            object_type: "blob".into(),
            body: b"binary\0payload\n".to_vec(),
        },
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackReadParity {
    pub oid: String,
    pub format: ObjectFormat,
    pub object_type: String,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackIndexParity {
    pub oid: String,
    pub format: ObjectFormat,
    pub offset: u64,
    pub entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaPackReadParity {
    pub format: ObjectFormat,
    pub entries: usize,
    pub delta_entries: usize,
    pub base_oid: String,
    pub changed_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinPackReadParity {
    pub format: ObjectFormat,
    pub entries: usize,
    pub base_oid: String,
    pub changed_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefInteropParity {
    pub format: ObjectFormat,
    pub name: String,
    pub oid: String,
    pub upstream_show_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeeledPackedRefInteropParity {
    pub format: ObjectFormat,
    pub name: String,
    pub tag_oid: String,
    pub peeled_oid: String,
    pub upstream_show_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowRefFilterParity {
    pub format: ObjectFormat,
    pub heads_upstream: String,
    pub heads_rust: String,
    pub tags_upstream: String,
    pub tags_rust: String,
    pub heads_hash_upstream: String,
    pub heads_hash_rust: String,
    pub tags_hash_upstream: String,
    pub tags_hash_rust: String,
    pub heads_abbrev_upstream: String,
    pub heads_abbrev_rust: String,
    pub tags_hash_abbrev_upstream: String,
    pub tags_hash_abbrev_rust: String,
    pub tags_deref_upstream: String,
    pub tags_deref_rust: String,
    pub tags_deref_hash_upstream: String,
    pub tags_deref_hash_rust: String,
    pub tags_deref_abbrev_upstream: String,
    pub tags_deref_abbrev_rust: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowRefVerifyParity {
    pub format: ObjectFormat,
    pub upstream: String,
    pub rust: String,
    pub hash_upstream: String,
    pub hash_rust: String,
    pub deref_upstream: String,
    pub deref_rust: String,
    pub quiet_upstream: Vec<u8>,
    pub quiet_rust: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolicRefParity {
    pub format: ObjectFormat,
    pub head_upstream: String,
    pub head_rust: String,
    pub short_upstream: String,
    pub short_rust: String,
    pub switched_upstream: String,
    pub switched_rust: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackWriteParity {
    pub oid: String,
    pub format: ObjectFormat,
    pub pack_name: String,
    pub upstream_body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaPackWriteParity {
    pub format: ObjectFormat,
    pub pack_name: String,
    pub base_oid: String,
    pub changed_oid: String,
    pub delta_entries: usize,
    pub upstream_body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleWriteParity {
    pub oid: String,
    pub format: ObjectFormat,
    pub heads: String,
    pub verify_stdout: String,
    pub upstream_body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LooseObjectInteropParity {
    pub oid: String,
    pub format: ObjectFormat,
    pub upstream_type: String,
    pub upstream_body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedOdbInteropParity {
    pub oid: String,
    pub format: ObjectFormat,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigInteropParity {
    pub object_format: ObjectFormat,
    pub bare: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsTreeParity {
    pub format: ObjectFormat,
    pub tree_oid: String,
    pub upstream: String,
    pub rust: String,
    pub name_only_upstream: String,
    pub name_only_rust: String,
    pub object_only_upstream: String,
    pub object_only_rust: String,
    pub long_upstream: String,
    pub long_rust: String,
    pub recursive_upstream: String,
    pub recursive_rust: String,
    pub recursive_object_only_upstream: String,
    pub recursive_object_only_rust: String,
    pub recursive_long_upstream: String,
    pub recursive_long_rust: String,
    pub recursive_name_only_upstream: String,
    pub recursive_name_only_rust: String,
    pub z_upstream: Vec<u8>,
    pub z_rust: Vec<u8>,
    pub name_only_z_upstream: Vec<u8>,
    pub name_only_z_rust: Vec<u8>,
    pub object_only_z_upstream: Vec<u8>,
    pub object_only_z_rust: Vec<u8>,
    pub long_z_upstream: Vec<u8>,
    pub long_z_rust: Vec<u8>,
    pub recursive_z_upstream: Vec<u8>,
    pub recursive_z_rust: Vec<u8>,
    pub recursive_object_only_z_upstream: Vec<u8>,
    pub recursive_object_only_z_rust: Vec<u8>,
    pub recursive_long_z_upstream: Vec<u8>,
    pub recursive_long_z_rust: Vec<u8>,
    pub recursive_name_only_z_upstream: Vec<u8>,
    pub recursive_name_only_z_rust: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogParity {
    pub format: ObjectFormat,
    pub commit_oid: String,
    pub upstream: String,
    pub rust: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatFileRevisionParity {
    pub format: ObjectFormat,
    pub revs: Vec<String>,
    pub upstream: Vec<Vec<u8>>,
    pub rust: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexParity {
    pub format: ObjectFormat,
    pub entries: usize,
    pub byte_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateIndexParity {
    pub format: ObjectFormat,
    pub upstream: String,
    pub expected: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsFilesStageParity {
    pub format: ObjectFormat,
    pub upstream: String,
    pub rust: String,
    pub upstream_z: Vec<u8>,
    pub rust_z: Vec<u8>,
    pub upstream_stage_z: Vec<u8>,
    pub rust_stage_z: Vec<u8>,
    pub upstream_others: String,
    pub rust_others: String,
    pub upstream_others_z: Vec<u8>,
    pub rust_others_z: Vec<u8>,
    pub upstream_stage_others: String,
    pub rust_stage_others: String,
    pub upstream_deleted: String,
    pub rust_deleted: String,
    pub upstream_deleted_z: Vec<u8>,
    pub rust_deleted_z: Vec<u8>,
    pub upstream_stage_deleted: String,
    pub rust_stage_deleted: String,
    pub upstream_others_deleted: String,
    pub rust_others_deleted: String,
    pub upstream_stage_others_deleted: String,
    pub rust_stage_others_deleted: String,
    pub upstream_modified: String,
    pub rust_modified: String,
    pub upstream_modified_z: Vec<u8>,
    pub rust_modified_z: Vec<u8>,
    pub upstream_stage_modified: String,
    pub rust_stage_modified: String,
    pub upstream_deleted_modified: String,
    pub rust_deleted_modified: String,
    pub upstream_stage_others_deleted_modified: String,
    pub rust_stage_others_deleted_modified: String,
    pub upstream_cached: String,
    pub rust_cached: String,
    pub upstream_cached_z: Vec<u8>,
    pub rust_cached_z: Vec<u8>,
    pub upstream_cached_others: String,
    pub rust_cached_others: String,
    pub upstream_cached_modified: String,
    pub rust_cached_modified: String,
    pub upstream_cached_deleted_modified: String,
    pub rust_cached_deleted_modified: String,
    pub upstream_deduplicate_deleted_modified: String,
    pub rust_deduplicate_deleted_modified: String,
    pub upstream_deduplicate_cached_modified: String,
    pub rust_deduplicate_cached_modified: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRefDeleteParity {
    pub format: ObjectFormat,
    pub before: String,
    pub after: String,
    pub deleted_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogExpireParity {
    pub format: ObjectFormat,
    pub before: String,
    pub after: String,
    pub removed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteTreeParity {
    pub format: ObjectFormat,
    pub upstream: String,
    pub rust: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitTreeParity {
    pub format: ObjectFormat,
    pub upstream: String,
    pub rust: String,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIndexParity {
    pub format: ObjectFormat,
    pub head: String,
    pub updated_ref: String,
    pub log: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddStatusParity {
    pub format: ObjectFormat,
    pub upstream: String,
    pub rust: String,
    pub porcelain_upstream: String,
    pub porcelain_rust: String,
    pub porcelain_branch_upstream: String,
    pub porcelain_branch_rust: String,
    pub porcelain_z_upstream: Vec<u8>,
    pub porcelain_z_rust: Vec<u8>,
    pub porcelain_branch_z_upstream: Vec<u8>,
    pub porcelain_branch_z_rust: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchParity {
    pub format: ObjectFormat,
    pub upstream: String,
    pub expected: String,
    pub points_at_upstream: String,
    pub points_at_expected: String,
    pub points_at_oid_upstream: String,
    pub points_at_oid_expected: String,
    pub remotes_upstream: String,
    pub remotes_expected: String,
    pub all_upstream: String,
    pub all_expected: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchShowCurrentParity {
    pub format: ObjectFormat,
    pub upstream: String,
    pub rust: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchDeleteParity {
    pub format: ObjectFormat,
    pub before: String,
    pub after: String,
    pub deleted_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutParity {
    pub format: ObjectFormat,
    pub branch: String,
    pub head: String,
    pub body: Vec<u8>,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagParity {
    pub format: ObjectFormat,
    pub upstream: String,
    pub expected: String,
    pub show_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagDeleteParity {
    pub format: ObjectFormat,
    pub before: String,
    pub after: String,
    pub deleted_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotatedTagParity {
    pub format: ObjectFormat,
    pub tag_oid: String,
    pub target_oid: String,
    pub upstream_type: String,
    pub upstream_body: Vec<u8>,
    pub expected_body: Vec<u8>,
    pub show_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffNameStatusParity {
    pub format: ObjectFormat,
    pub upstream: String,
    pub rust: String,
    pub name_only_upstream: String,
    pub name_only_rust: String,
    pub cached_upstream: String,
    pub cached_rust: String,
    pub cached_name_only_upstream: String,
    pub cached_name_only_rust: String,
    pub rename_copy_upstream: String,
    pub rename_copy_rust: String,
    pub rename_copy_name_only_upstream: String,
    pub rename_copy_name_only_rust: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevParseParity {
    pub format: ObjectFormat,
    pub upstream: String,
    pub rust: String,
    pub short_upstream: String,
    pub short_rust: String,
    pub short_8_upstream: String,
    pub short_8_rust: String,
    pub short_min_upstream: String,
    pub short_min_rust: String,
    pub verify_upstream: String,
    pub verify_rust: String,
    pub verify_quiet_upstream: String,
    pub verify_quiet_rust: String,
    pub verify_short_upstream: String,
    pub verify_short_rust: String,
    pub abbrev_ref_upstream: String,
    pub abbrev_ref_rust: String,
    pub symbolic_full_name_upstream: String,
    pub symbolic_full_name_rust: String,
    pub top_level_upstream: String,
    pub top_level_rust: String,
    pub prefix_root_upstream: String,
    pub prefix_root_rust: String,
    pub prefix_nested_upstream: String,
    pub prefix_nested_rust: String,
    pub cdup_root_upstream: String,
    pub cdup_root_rust: String,
    pub cdup_nested_upstream: String,
    pub cdup_nested_rust: String,
    pub git_dir_upstream: String,
    pub git_dir_rust: String,
    pub absolute_git_dir_upstream: String,
    pub absolute_git_dir_rust: String,
    pub inside_work_tree_upstream: String,
    pub inside_work_tree_rust: String,
    pub inside_git_dir_worktree_upstream: String,
    pub inside_git_dir_worktree_rust: String,
    pub inside_git_dir_git_upstream: String,
    pub inside_git_dir_git_rust: String,
    pub inside_git_dir_bare_upstream: String,
    pub inside_git_dir_bare_rust: String,
    pub bare_worktree_upstream: String,
    pub bare_worktree_rust: String,
    pub bare_repo_upstream: String,
    pub bare_repo_rust: String,
    pub shallow_worktree_upstream: String,
    pub shallow_worktree_rust: String,
    pub shallow_marker_upstream: String,
    pub shallow_marker_rust: String,
    pub shallow_bare_upstream: String,
    pub shallow_bare_rust: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevParseObjectFormatParity {
    pub sha1_upstream: String,
    pub sha1_rust: String,
    pub sha256_upstream: String,
    pub sha256_rust: String,
}

pub fn single_blob_pack_read_parity() -> Result<PackReadParity> {
    single_blob_pack_read_parity_for_format(ObjectFormat::Sha1)
}

pub fn single_blob_pack_read_parity_sha256() -> Result<PackReadParity> {
    single_blob_pack_read_parity_for_format(ObjectFormat::Sha256)
}

fn single_blob_pack_read_parity_for_format(format: ObjectFormat) -> Result<PackReadParity> {
    let root = unique_temp_dir("sley-pack-read");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<PackReadParity> {
        init_repo_for_format(&root, format)?;
        let body = b"hello from pack\n";
        let oid = run_git(&root, ["hash-object", "-w", "--stdin"], body)?;
        let pack_bytes = run_git(&root, ["pack-objects", "--stdout"], &oid)?;
        let pack = PackFile::parse(&pack_bytes, format)?;
        if pack.entries.len() != 1 {
            return Err(GitError::InvalidFormat(format!(
                "expected one pack entry, found {}",
                pack.entries.len()
            )));
        }
        let entry = &pack.entries[0];
        if entry.entry.oid.to_hex() != String::from_utf8_lossy(&oid).trim() {
            return Err(GitError::InvalidFormat("pack entry oid mismatch".into()));
        }
        if entry.object.body != body {
            return Err(GitError::InvalidObject(
                "pack entry body does not match upstream blob".into(),
            ));
        }
        Ok(PackReadParity {
            oid: entry.entry.oid.to_hex(),
            format,
            object_type: entry.object.object_type.as_str().into(),
            body: entry.object.body.clone(),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn single_blob_pack_index_parity() -> Result<PackIndexParity> {
    single_blob_pack_index_parity_for_format(ObjectFormat::Sha1)
}

pub fn single_blob_pack_index_parity_sha256() -> Result<PackIndexParity> {
    single_blob_pack_index_parity_for_format(ObjectFormat::Sha256)
}

fn single_blob_pack_index_parity_for_format(format: ObjectFormat) -> Result<PackIndexParity> {
    let root = unique_temp_dir("sley-pack-index");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<PackIndexParity> {
        init_repo_for_format(&root, format)?;
        let body = b"hello from indexed pack\n";
        let oid = run_git(&root, ["hash-object", "-w", "--stdin"], body)?;
        let pack_hash = run_git(&root, ["pack-objects", ".git/objects/pack/pack"], &oid)?;
        let pack_hash = String::from_utf8_lossy(&pack_hash).trim().to_string();
        let oid = String::from_utf8_lossy(&oid).trim().to_string();
        let pack_path = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join(format!("pack-{pack_hash}.pack"));
        let index_path = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join(format!("pack-{pack_hash}.idx"));
        let pack = PackFile::parse(&fs::read(pack_path)?, format)?;
        let index = PackIndex::parse(&fs::read(index_path)?, format)?;
        if index.entries.len() != 1 {
            return Err(GitError::InvalidFormat(format!(
                "expected one pack index entry, found {}",
                index.entries.len()
            )));
        }
        if index.pack_checksum != pack.checksum {
            return Err(GitError::InvalidFormat(
                "pack index trailer does not match pack checksum".into(),
            ));
        }
        let oid = sley_core::ObjectId::from_hex(format, &oid)?;
        let Some(entry) = index.find(&oid) else {
            return Err(GitError::not_found(format!(
                "object {oid} not found in generated pack index"
            )));
        };
        Ok(PackIndexParity {
            oid: oid.to_hex(),
            format,
            offset: entry.offset,
            entries: index.entries.len(),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn delta_pack_read_parity() -> Result<DeltaPackReadParity> {
    delta_pack_read_parity_for_format(ObjectFormat::Sha1)
}

pub fn delta_pack_read_parity_sha256() -> Result<DeltaPackReadParity> {
    delta_pack_read_parity_for_format(ObjectFormat::Sha256)
}

fn delta_pack_read_parity_for_format(format: ObjectFormat) -> Result<DeltaPackReadParity> {
    let root = unique_temp_dir("sley-pack-delta");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<DeltaPackReadParity> {
        init_repo_for_format(&root, format)?;
        let base_body = repeated_blob_body("common payload\n", "base payload\n");
        let changed_body = repeated_blob_body("common payload\n", "changed payload\n");
        let base_oid = run_git(&root, ["hash-object", "-w", "--stdin"], &base_body)?;
        let changed_oid = run_git(&root, ["hash-object", "-w", "--stdin"], &changed_body)?;
        let oid_input = [base_oid.as_slice(), changed_oid.as_slice()].concat();
        let pack_hash = run_git(
            &root,
            [
                "pack-objects",
                "--window=10",
                "--depth=50",
                ".git/objects/pack/pack",
            ],
            &oid_input,
        )?;
        let pack_hash = String::from_utf8_lossy(&pack_hash).trim().to_string();
        let pack_path = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join(format!("pack-{pack_hash}.pack"));
        let index_path = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join(format!("pack-{pack_hash}.idx"));
        let verify = run_git_owned(
            &root,
            &[
                "verify-pack".into(),
                "-v".into(),
                index_path.to_string_lossy().into_owned(),
            ],
            &[],
        )?;
        let delta_entries = String::from_utf8_lossy(&verify)
            .lines()
            .filter(|line| {
                let fields = line.split_whitespace().count();
                fields >= 7 && line.contains(" blob ")
            })
            .count();
        if delta_entries == 0 {
            return Err(GitError::InvalidFormat(
                "upstream pack did not contain a deltified blob".into(),
            ));
        }
        let pack = PackFile::parse(&fs::read(pack_path)?, format)?;
        if pack.entries.len() != 2 {
            return Err(GitError::InvalidFormat(format!(
                "expected two pack entries, found {}",
                pack.entries.len()
            )));
        }
        if !pack
            .entries
            .iter()
            .any(|entry| entry.object.body == base_body)
            || !pack
                .entries
                .iter()
                .any(|entry| entry.object.body == changed_body)
        {
            return Err(GitError::InvalidObject(
                "delta pack parser did not reconstruct expected blob bodies".into(),
            ));
        }
        Ok(DeltaPackReadParity {
            format,
            entries: pack.entries.len(),
            delta_entries,
            base_oid: String::from_utf8_lossy(&base_oid).trim().to_string(),
            changed_oid: String::from_utf8_lossy(&changed_oid).trim().to_string(),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn repeated_blob_body(common_line: &str, final_line: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for _ in 0..400 {
        body.extend_from_slice(common_line.as_bytes());
    }
    body.extend_from_slice(final_line.as_bytes());
    body
}

pub fn thin_pack_read_parity() -> Result<ThinPackReadParity> {
    thin_pack_read_parity_for_format(ObjectFormat::Sha1)
}

pub fn thin_pack_read_parity_sha256() -> Result<ThinPackReadParity> {
    thin_pack_read_parity_for_format(ObjectFormat::Sha256)
}

pub fn thin_pack_read_parity_for_format(format: ObjectFormat) -> Result<ThinPackReadParity> {
    let root = unique_temp_dir("sley-pack-thin");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<ThinPackReadParity> {
        init_repo_for_format(&root, format)?;
        let base_body = repeated_blob_body("common payload\n", "base payload\n");
        let changed_body = repeated_blob_body("common payload\n", "changed payload\n");
        fs::write(root.join("payload.txt"), &base_body)?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("payload.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let base_commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity.clone(),
                message: b"base\n".to_vec(),
                reflog_message: b"commit: base".to_vec(),
                encoding: None,
            signature: None,
            },
        )?
        .oid
        .to_hex();
        fs::write(root.join("payload.txt"), &changed_body)?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("payload.txt")],
        )?;
        let changed_commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity,
                message: b"changed\n".to_vec(),
                reflog_message: b"commit: changed".to_vec(),
                encoding: None,
            signature: None,
            },
        )?
        .oid
        .to_hex();
        let base_oid = sley_core::object_id_for_bytes(format, "blob", &base_body)?.to_hex();
        let changed_oid = sley_core::object_id_for_bytes(format, "blob", &changed_body)?.to_hex();
        let thin_input = format!("{changed_commit}\n^{base_commit}\n").into_bytes();
        let pack_bytes = run_git(
            &root,
            [
                "pack-objects",
                "--revs",
                "--thin",
                "--window=10",
                "--depth=50",
                "--stdout",
            ],
            &thin_input,
        )?;
        if PackFile::parse(&pack_bytes, format).is_ok() {
            return Err(GitError::InvalidFormat(
                "upstream thin pack unexpectedly parsed without external base".into(),
            ));
        }
        let object_db = FileObjectDatabase::from_git_dir(root.join(".git"), format);
        let parsed = PackFile::parse_thin(&pack_bytes, format, |oid| {
            match object_db.read_object(oid) {
                Ok(object) => Ok(Some((*object).clone())),
                Err(GitError::NotFound(_)) => Ok(None),
                Err(err) => Err(err),
            }
        })?;
        if !parsed
            .entries
            .iter()
            .any(|entry| entry.object.body == changed_body)
        {
            return Err(GitError::InvalidObject(
                "thin pack parser did not reconstruct expected blob body".into(),
            ));
        }
        Ok(ThinPackReadParity {
            format,
            entries: parsed.entries.len(),
            base_oid,
            changed_oid,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn loose_ref_interop_parity() -> Result<RefInteropParity> {
    loose_ref_interop_parity_for_format(ObjectFormat::Sha1)
}

pub fn loose_ref_interop_parity_sha256() -> Result<RefInteropParity> {
    loose_ref_interop_parity_for_format(ObjectFormat::Sha256)
}

pub fn loose_ref_interop_parity_for_format(format: ObjectFormat) -> Result<RefInteropParity> {
    let root = unique_temp_dir("sley-ref-interop");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<RefInteropParity> {
        init_repo_for_format(&root, format)?;
        let oid = String::from_utf8_lossy(&run_git(
            &root,
            ["hash-object", "-w", "--stdin"],
            b"ref interop\n",
        )?)
        .trim()
        .to_string();
        let oid = ObjectId::from_hex(format, &oid)?;
        let name = "refs/heads/sley-ref-interop";
        let store = FileRefStore::new(root.join(".git"), format);
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: name.into(),
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(format)?,
                new_oid: oid,
                committer: b"Git Rs <sley@example.invalid> 0 +0000".to_vec(),
                message: b"interop".to_vec(),
            }),
        });
        tx.commit()?;
        let upstream =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", name], &[])?).to_string();
        let expected = format!("{oid} {name}\n");
        if upstream != expected {
            return Err(GitError::Command(format!(
                "show-ref mismatch: expected {expected:?}, got {upstream:?}"
            )));
        }
        Ok(RefInteropParity {
            format,
            name: name.into(),
            oid: oid.to_hex(),
            upstream_show_ref: upstream,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn packed_ref_interop_parity() -> Result<RefInteropParity> {
    packed_ref_interop_parity_for_format(ObjectFormat::Sha1)
}

pub fn packed_ref_interop_parity_sha256() -> Result<RefInteropParity> {
    packed_ref_interop_parity_for_format(ObjectFormat::Sha256)
}

pub fn packed_ref_interop_parity_for_format(format: ObjectFormat) -> Result<RefInteropParity> {
    let root = unique_temp_dir("sley-packed-ref-interop");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<RefInteropParity> {
        init_repo_for_format(&root, format)?;
        let oid = String::from_utf8_lossy(&run_git(
            &root,
            ["hash-object", "-w", "--stdin"],
            b"packed ref interop\n",
        )?)
        .trim()
        .to_string();
        let oid = ObjectId::from_hex(format, &oid)?;
        let name = "refs/heads/sley-packed-ref-interop";
        let store = FileRefStore::new(root.join(".git"), format);
        store.write_packed_refs(&[PackedRef {
            reference: Ref {
                name: name.into(),
                target: RefTarget::Direct(oid),
            },
            peeled: None,
        }])?;
        let upstream =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", name], &[])?).to_string();
        let expected = format!("{oid} {name}\n");
        if upstream != expected {
            return Err(GitError::Command(format!(
                "show-ref mismatch for packed ref: expected {expected:?}, got {upstream:?}"
            )));
        }
        Ok(RefInteropParity {
            format,
            name: name.into(),
            oid: oid.to_hex(),
            upstream_show_ref: upstream,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn packed_ref_compaction_interop_parity() -> Result<RefInteropParity> {
    packed_ref_compaction_interop_parity_for_format(ObjectFormat::Sha1)
}

pub fn packed_ref_compaction_interop_parity_sha256() -> Result<RefInteropParity> {
    packed_ref_compaction_interop_parity_for_format(ObjectFormat::Sha256)
}

pub fn packed_ref_compaction_interop_parity_for_format(
    format: ObjectFormat,
) -> Result<RefInteropParity> {
    let root = unique_temp_dir("sley-packed-ref-compaction");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<RefInteropParity> {
        init_repo_for_format(&root, format)?;
        let oid = String::from_utf8_lossy(&run_git(
            &root,
            ["hash-object", "-w", "--stdin"],
            b"packed ref compaction\n",
        )?)
        .trim()
        .to_string();
        let oid = ObjectId::from_hex(format, &oid)?;
        let name = "refs/heads/sley-packed-ref-compaction";
        let store = FileRefStore::new(root.join(".git"), format);
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: name.into(),
            expected: None,
            new: RefTarget::Direct(oid),
            reflog: None,
        });
        tx.commit()?;
        let packed = store.pack_refs(true)?;
        if !packed.iter().any(|packed| packed.reference.name == name) {
            return Err(GitError::InvalidFormat(
                "packed refs did not include compacted loose ref".into(),
            ));
        }
        if root.join(".git").join(name).exists() {
            return Err(GitError::InvalidFormat(
                "pack_refs(true) did not prune loose ref".into(),
            ));
        }
        let upstream =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", name], &[])?).to_string();
        let expected = format!("{oid} {name}\n");
        if upstream != expected {
            return Err(GitError::Command(format!(
                "show-ref mismatch for compacted packed ref: expected {expected:?}, got {upstream:?}"
            )));
        }
        Ok(RefInteropParity {
            format,
            name: name.into(),
            oid: oid.to_hex(),
            upstream_show_ref: upstream,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn peeled_packed_ref_compaction_interop_parity() -> Result<PeeledPackedRefInteropParity> {
    peeled_packed_ref_compaction_interop_parity_for_format(ObjectFormat::Sha1)
}

pub fn peeled_packed_ref_compaction_interop_parity_sha256() -> Result<PeeledPackedRefInteropParity>
{
    peeled_packed_ref_compaction_interop_parity_for_format(ObjectFormat::Sha256)
}

pub fn peeled_packed_ref_compaction_interop_parity_for_format(
    format: ObjectFormat,
) -> Result<PeeledPackedRefInteropParity> {
    let root = unique_temp_dir("sley-peeled-packed-ref-compaction");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<PeeledPackedRefInteropParity> {
        init_repo_for_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity.clone(),
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let mut db = FileObjectDatabase::from_git_dir(root.join(".git"), format);
        let tag_oid = sley_sequencer::create_annotated_tag(
            &mut db,
            sley_sequencer::TagCreate {
                object: commit.oid,
                object_type: ObjectType::Commit,
                name: b"v-peel".to_vec(),
                tagger: identity,
                message: b"peeled tag\n".to_vec(),
            },
        )?;
        let name = "refs/tags/v-peel";
        let store = FileRefStore::new(root.join(".git"), format);
        store.create_tag("v-peel", tag_oid)?;
        let packed = sley_rev::pack_refs_with_auto_peel(root.join(".git"), format, true)?;
        let Some(packed_tag) = packed.iter().find(|packed| packed.reference.name == name) else {
            return Err(GitError::InvalidFormat(
                "packed refs did not include annotated tag".into(),
            ));
        };
        let peeled_oid = packed_tag.peeled.clone().ok_or_else(|| {
            GitError::InvalidFormat("packed annotated tag did not include peeled oid".into())
        })?;
        if root.join(".git").join(name).exists() {
            return Err(GitError::InvalidFormat(
                "pack_refs_with_peeler(true) did not prune loose tag".into(),
            ));
        }
        let upstream =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "-d", name], &[])?).to_string();
        let expected = format!("{tag_oid} {name}\n{peeled_oid} {name}^{{}}\n");
        if upstream != expected {
            return Err(GitError::Command(format!(
                "show-ref -d mismatch for peeled packed ref: expected {expected:?}, got {upstream:?}"
            )));
        }
        Ok(PeeledPackedRefInteropParity {
            format,
            name: name.into(),
            tag_oid: tag_oid.to_hex(),
            peeled_oid: peeled_oid.to_hex(),
            upstream_show_ref: upstream,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn symbolic_ref_parity() -> Result<SymbolicRefParity> {
    symbolic_ref_parity_for_format(ObjectFormat::Sha1)
}

pub fn symbolic_ref_parity_sha256() -> Result<SymbolicRefParity> {
    symbolic_ref_parity_for_format(ObjectFormat::Sha256)
}

pub fn symbolic_ref_parity_for_format(format: ObjectFormat) -> Result<SymbolicRefParity> {
    let root = unique_temp_dir("sley-symbolic-ref");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<SymbolicRefParity> {
        init_repo_with_format(&root, format)?;
        let store = FileRefStore::new(root.join(".git"), format);
        let head_upstream =
            String::from_utf8_lossy(&run_git(&root, ["symbolic-ref", "HEAD"], &[])?).to_string();
        let short_upstream =
            String::from_utf8_lossy(&run_git(&root, ["symbolic-ref", "--short", "HEAD"], &[])?)
                .to_string();
        let head_rust = match store.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(target)) => format!("{target}\n"),
            _ => {
                return Err(GitError::Command(
                    "HEAD was not symbolic in upstream-created repository".into(),
                ));
            }
        };
        let short_rust = match store.current_branch()? {
            Some(branch) => format!("{branch}\n"),
            None => {
                return Err(GitError::Command(
                    "HEAD did not point at a local branch".into(),
                ));
            }
        };
        if head_rust != head_upstream || short_rust != short_upstream {
            return Err(GitError::Command(format!(
                "symbolic-ref mismatch: head expected {head_upstream:?}, got {head_rust:?}; short expected {short_upstream:?}, got {short_rust:?}"
            )));
        }
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/feature".into()),
            reflog: None,
        });
        tx.commit()?;
        let switched_upstream =
            String::from_utf8_lossy(&run_git(&root, ["symbolic-ref", "HEAD"], &[])?).to_string();
        let switched_rust = "refs/heads/feature\n".to_string();
        if switched_rust != switched_upstream {
            return Err(GitError::Command(format!(
                "symbolic-ref write mismatch: expected {switched_upstream:?}, got {switched_rust:?}"
            )));
        }
        Ok(SymbolicRefParity {
            format,
            head_upstream,
            head_rust,
            short_upstream,
            short_rust,
            switched_upstream,
            switched_rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn show_ref_filter_parity() -> Result<ShowRefFilterParity> {
    show_ref_filter_parity_for_format(ObjectFormat::Sha1)
}

pub fn show_ref_filter_parity_sha256() -> Result<ShowRefFilterParity> {
    show_ref_filter_parity_for_format(ObjectFormat::Sha256)
}

pub fn show_ref_filter_parity_for_format(format: ObjectFormat) -> Result<ShowRefFilterParity> {
    let root = unique_temp_dir("sley-show-ref-filter");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<ShowRefFilterParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity.clone(),
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let store = FileRefStore::new(root.join(".git"), format);
        store.create_branch(
            "feature",
            commit.oid,
            identity.clone(),
            b"branch: Created from main".to_vec(),
        )?;
        let mut db = FileObjectDatabase::from_git_dir(root.join(".git"), format);
        let tag_oid = sley_sequencer::create_annotated_tag(
            &mut db,
            sley_sequencer::TagCreate {
                object: commit.oid,
                object_type: ObjectType::Commit,
                name: b"v2.0".to_vec(),
                tagger: identity,
                message: b"release v2\n".to_vec(),
            },
        )?;
        store.create_tag("v1.0", commit.oid)?;
        store.create_tag("v2.0", tag_oid)?;
        let heads_upstream =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "--heads"], &[])?).to_string();
        let tags_upstream =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "--tags"], &[])?).to_string();
        let heads_hash_upstream =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "--hash", "--heads"], &[])?)
                .to_string();
        let tags_hash_upstream =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "-s", "--tags"], &[])?)
                .to_string();
        let heads_abbrev_upstream =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "--abbrev=8", "--heads"], &[])?)
                .to_string();
        let tags_hash_abbrev_upstream =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "--hash=8", "--tags"], &[])?)
                .to_string();
        let tags_deref_upstream =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "-d", "--tags"], &[])?)
                .to_string();
        let tags_deref_hash_upstream = String::from_utf8_lossy(&run_git(
            &root,
            ["show-ref", "-d", "--hash", "--tags"],
            &[],
        )?)
        .to_string();
        let tags_deref_abbrev_upstream = String::from_utf8_lossy(&run_git(
            &root,
            ["show-ref", "-d", "--abbrev=8", "--tags"],
            &[],
        )?)
        .to_string();
        let refs = store.list_refs()?;
        let heads_rust = format_show_refs(&refs, "refs/heads/");
        let tags_rust = format_show_refs(&refs, "refs/tags/");
        let heads_hash_rust = format_show_refs_with_options(&refs, "refs/heads/", true, None);
        let tags_hash_rust = format_show_refs_with_options(&refs, "refs/tags/", true, None);
        let heads_abbrev_rust = format_show_refs_with_options(&refs, "refs/heads/", false, Some(8));
        let tags_hash_abbrev_rust =
            format_show_refs_with_options(&refs, "refs/tags/", true, Some(8));
        let tags_deref_rust =
            format_show_refs_deref(&db, format, &refs, "refs/tags/", false, None)?;
        let tags_deref_hash_rust =
            format_show_refs_deref(&db, format, &refs, "refs/tags/", true, None)?;
        let tags_deref_abbrev_rust =
            format_show_refs_deref(&db, format, &refs, "refs/tags/", false, Some(8))?;
        if heads_rust != heads_upstream
            || tags_rust != tags_upstream
            || heads_hash_rust != heads_hash_upstream
            || tags_hash_rust != tags_hash_upstream
            || heads_abbrev_rust != heads_abbrev_upstream
            || tags_hash_abbrev_rust != tags_hash_abbrev_upstream
            || tags_deref_rust != tags_deref_upstream
            || tags_deref_hash_rust != tags_deref_hash_upstream
            || tags_deref_abbrev_rust != tags_deref_abbrev_upstream
        {
            return Err(GitError::Command(format!(
                "show-ref filter mismatch: heads expected {heads_upstream:?}, got {heads_rust:?}; tags expected {tags_upstream:?}, got {tags_rust:?}; heads hash expected {heads_hash_upstream:?}, got {heads_hash_rust:?}; tags hash expected {tags_hash_upstream:?}, got {tags_hash_rust:?}; heads abbrev expected {heads_abbrev_upstream:?}, got {heads_abbrev_rust:?}; tags hash abbrev expected {tags_hash_abbrev_upstream:?}, got {tags_hash_abbrev_rust:?}; tags deref expected {tags_deref_upstream:?}, got {tags_deref_rust:?}; tags deref hash expected {tags_deref_hash_upstream:?}, got {tags_deref_hash_rust:?}; tags deref abbrev expected {tags_deref_abbrev_upstream:?}, got {tags_deref_abbrev_rust:?}"
            )));
        }
        Ok(ShowRefFilterParity {
            format,
            heads_upstream,
            heads_rust,
            tags_upstream,
            tags_rust,
            heads_hash_upstream,
            heads_hash_rust,
            tags_hash_upstream,
            tags_hash_rust,
            heads_abbrev_upstream,
            heads_abbrev_rust,
            tags_hash_abbrev_upstream,
            tags_hash_abbrev_rust,
            tags_deref_upstream,
            tags_deref_rust,
            tags_deref_hash_upstream,
            tags_deref_hash_rust,
            tags_deref_abbrev_upstream,
            tags_deref_abbrev_rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn show_ref_verify_parity() -> Result<ShowRefVerifyParity> {
    show_ref_verify_parity_for_format(ObjectFormat::Sha1)
}

pub fn show_ref_verify_parity_sha256() -> Result<ShowRefVerifyParity> {
    show_ref_verify_parity_for_format(ObjectFormat::Sha256)
}

pub fn show_ref_verify_parity_for_format(format: ObjectFormat) -> Result<ShowRefVerifyParity> {
    let root = unique_temp_dir("sley-show-ref-verify");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<ShowRefVerifyParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity.clone(),
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let store = FileRefStore::new(root.join(".git"), format);
        let mut db = FileObjectDatabase::from_git_dir(root.join(".git"), format);
        let tag_oid = sley_sequencer::create_annotated_tag(
            &mut db,
            sley_sequencer::TagCreate {
                object: commit.oid,
                object_type: ObjectType::Commit,
                name: b"v2.0".to_vec(),
                tagger: identity,
                message: b"release v2\n".to_vec(),
            },
        )?;
        store.create_tag("v1.0", commit.oid)?;
        store.create_tag("v2.0", tag_oid)?;
        let upstream = String::from_utf8_lossy(&run_git(
            &root,
            ["show-ref", "--verify", "refs/heads/main", "refs/tags/v1.0"],
            &[],
        )?)
        .to_string();
        let rust = format!(
            "{} refs/heads/main\n{} refs/tags/v1.0\n",
            commit.oid, commit.oid
        );
        if rust != upstream {
            return Err(GitError::Command(format!(
                "show-ref --verify mismatch: expected {upstream:?}, got {rust:?}"
            )));
        }
        let hash_upstream = String::from_utf8_lossy(&run_git(
            &root,
            [
                "show-ref",
                "--verify",
                "--hash",
                "refs/heads/main",
                "refs/tags/v1.0",
            ],
            &[],
        )?)
        .to_string();
        let hash_rust = format!("{}\n{}\n", commit.oid, commit.oid);
        if hash_rust != hash_upstream {
            return Err(GitError::Command(format!(
                "show-ref --verify --hash mismatch: expected {hash_upstream:?}, got {hash_rust:?}"
            )));
        }
        let deref_upstream = String::from_utf8_lossy(&run_git(
            &root,
            ["show-ref", "--verify", "-d", "refs/tags/v2.0"],
            &[],
        )?)
        .to_string();
        let deref_rust = format!(
            "{} refs/tags/v2.0\n{} refs/tags/v2.0^{{}}\n",
            tag_oid, commit.oid
        );
        if deref_rust != deref_upstream {
            return Err(GitError::Command(format!(
                "show-ref --verify -d mismatch: expected {deref_upstream:?}, got {deref_rust:?}"
            )));
        }
        let quiet_upstream = run_git(
            &root,
            ["show-ref", "--verify", "--quiet", "refs/heads/main"],
            &[],
        )?;
        let quiet_rust = Vec::new();
        if quiet_rust != quiet_upstream {
            return Err(GitError::Command(format!(
                "show-ref --verify --quiet mismatch: expected {quiet_upstream:?}, got {quiet_rust:?}"
            )));
        }
        Ok(ShowRefVerifyParity {
            format,
            upstream,
            rust,
            hash_upstream,
            hash_rust,
            deref_upstream,
            deref_rust,
            quiet_upstream,
            quiet_rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn format_show_refs(refs: &[sley_refs::Ref], prefix: &str) -> String {
    format_show_refs_with_options(refs, prefix, false, None)
}

fn format_show_refs_with_options(
    refs: &[sley_refs::Ref],
    prefix: &str,
    hash_only: bool,
    abbrev: Option<usize>,
) -> String {
    let mut out = String::new();
    for reference in refs {
        if !reference.name.starts_with(prefix) {
            continue;
        }
        if let RefTarget::Direct(oid) = &reference.target {
            let oid = oid.to_hex();
            let display_len = abbrev.unwrap_or(oid.len()).min(oid.len());
            let display_oid = &oid[..display_len];
            if hash_only {
                out.push_str(&format!("{display_oid}\n"));
            } else {
                out.push_str(&format!("{display_oid} {}\n", reference.name));
            }
        }
    }
    out
}

fn format_show_refs_deref(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    refs: &[sley_refs::Ref],
    prefix: &str,
    hash_only: bool,
    abbrev: Option<usize>,
) -> Result<String> {
    let mut out = String::new();
    for reference in refs {
        if !reference.name.starts_with(prefix) {
            continue;
        }
        let RefTarget::Direct(oid) = &reference.target else {
            continue;
        };
        push_show_ref_line(&mut out, oid, &reference.name, hash_only, abbrev);
        let object = db.read_object(oid)?;
        if object.object_type == ObjectType::Tag {
            let peeled = sley_rev::peel_tags(db, format, oid)?;
            push_show_ref_line(
                &mut out,
                &peeled,
                &format!("{}^{{}}", reference.name),
                false,
                abbrev,
            );
        }
    }
    Ok(out)
}

fn push_show_ref_line(
    out: &mut String,
    oid: &ObjectId,
    name: &str,
    hash_only: bool,
    abbrev: Option<usize>,
) {
    let oid = oid.to_hex();
    let display_len = abbrev.unwrap_or(oid.len()).min(oid.len());
    let display_oid = &oid[..display_len];
    if hash_only {
        out.push_str(&format!("{display_oid}\n"));
    } else {
        out.push_str(&format!("{display_oid} {name}\n"));
    }
}

pub fn rust_pack_write_interop_parity() -> Result<PackWriteParity> {
    rust_pack_write_interop_parity_for_format(ObjectFormat::Sha1)
}

pub fn rust_pack_write_interop_parity_sha256() -> Result<PackWriteParity> {
    rust_pack_write_interop_parity_for_format(ObjectFormat::Sha256)
}

fn rust_pack_write_interop_parity_for_format(format: ObjectFormat) -> Result<PackWriteParity> {
    let root = unique_temp_dir("sley-pack-write");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<PackWriteParity> {
        init_repo_for_format(&root, format)?;
        let body = b"hello from rust pack writer\n";
        let object = EncodedObject::new(ObjectType::Blob, body.to_vec());
        let oid = object.object_id(format)?;
        let written = PackFile::write_undeltified(&[object], format)?;
        let pack_name = written.checksum.to_hex();
        let pack_dir = root.join(".git").join("objects").join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_path = pack_dir.join(format!("pack-{pack_name}.pack"));
        let index_path = pack_dir.join(format!("pack-{pack_name}.idx"));
        fs::write(&pack_path, &written.pack)?;
        fs::write(&index_path, &written.index)?;
        run_git_owned(
            &root,
            &[
                "verify-pack".into(),
                "-v".into(),
                index_path.to_string_lossy().into_owned(),
            ],
            &[],
        )?;
        let upstream_body =
            run_git_owned(&root, &["cat-file".into(), "-p".into(), oid.to_hex()], &[])?;
        if upstream_body != body {
            return Err(GitError::Command(
                "upstream git read different body from rust-written pack".into(),
            ));
        }
        Ok(PackWriteParity {
            oid: oid.to_hex(),
            format,
            pack_name,
            upstream_body,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn rust_delta_pack_write_interop_parity() -> Result<DeltaPackWriteParity> {
    rust_delta_pack_write_interop_parity_for_format(ObjectFormat::Sha1)
}

pub fn rust_delta_pack_write_interop_parity_sha256() -> Result<DeltaPackWriteParity> {
    rust_delta_pack_write_interop_parity_for_format(ObjectFormat::Sha256)
}

pub fn rust_bundle_write_interop_parity() -> Result<BundleWriteParity> {
    rust_bundle_write_interop_parity_for_format(ObjectFormat::Sha1)
}

pub fn rust_bundle_write_interop_parity_sha256() -> Result<BundleWriteParity> {
    rust_bundle_write_interop_parity_for_format(ObjectFormat::Sha256)
}

fn rust_bundle_write_interop_parity_for_format(format: ObjectFormat) -> Result<BundleWriteParity> {
    let root = unique_temp_dir("sley-bundle-write");
    let source = root.join("source");
    let destination = root.join("destination");
    fs::create_dir_all(&source)?;
    fs::create_dir_all(&destination)?;
    let result = (|| -> Result<BundleWriteParity> {
        init_repo_for_format(&source, format)?;
        init_repo_for_format(&destination, format)?;
        let body = b"hello from rust bundle writer\n";
        let object = EncodedObject::new(ObjectType::Blob, body.to_vec());
        let oid = object.object_id(format)?;
        let written = PackFile::write_undeltified(&[object], format)?;
        let bundle = Bundle {
            version: if format == ObjectFormat::Sha1 { 2 } else { 3 },
            format,
            capabilities: Vec::new(),
            prerequisites: Vec::new(),
            references: vec![BundleReference {
                oid,
                name: "refs/heads/main".into(),
            }],
            pack: written.pack,
        };
        let bundle_path = root.join("rust.bundle");
        fs::write(&bundle_path, bundle.write()?)?;
        let bundle_arg = bundle_path.to_string_lossy().into_owned();
        let verify_stdout = run_git_owned(
            &source,
            &["bundle".into(), "verify".into(), bundle_arg.clone()],
            &[],
        )?;
        let heads = run_git_owned(
            &source,
            &["bundle".into(), "list-heads".into(), bundle_arg.clone()],
            &[],
        )?;
        let unbundle = run_git_owned(
            &destination,
            &["bundle".into(), "unbundle".into(), bundle_arg],
            &[],
        )?;
        if unbundle != heads {
            return Err(GitError::Command(
                "upstream git unbundle output differed from list-heads".into(),
            ));
        }
        let upstream_body = run_git_owned(
            &destination,
            &["cat-file".into(), "-p".into(), oid.to_hex()],
            &[],
        )?;
        if upstream_body != body {
            return Err(GitError::Command(
                "upstream git read different body from rust-written bundle".into(),
            ));
        }
        Ok(BundleWriteParity {
            oid: oid.to_hex(),
            format,
            heads: String::from_utf8_lossy(&heads).into_owned(),
            verify_stdout: String::from_utf8_lossy(&verify_stdout).into_owned(),
            upstream_body,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn rust_delta_pack_write_interop_parity_for_format(
    format: ObjectFormat,
) -> Result<DeltaPackWriteParity> {
    let root = unique_temp_dir("sley-pack-write-delta");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<DeltaPackWriteParity> {
        init_repo_for_format(&root, format)?;
        let base_body = repeated_blob_body("common payload\n", "base payload\n");
        let changed_body = repeated_blob_body("common payload\n", "changed payload\n");
        let base = EncodedObject::new(ObjectType::Blob, base_body);
        let changed = EncodedObject::new(ObjectType::Blob, changed_body.clone());
        let base_oid = base.object_id(format)?;
        let changed_oid = changed.object_id(format)?;
        let options = PackWriteOptions::new()
            .with_prefer_ofs_delta(false)
            .with_reorder(false);
        let written = PackFile::write_packed_with_options(&[base, changed], format, &options)?;
        let pack_name = written.checksum.to_hex();
        let pack_dir = root.join(".git").join("objects").join("pack");
        fs::create_dir_all(&pack_dir)?;
        let pack_path = pack_dir.join(format!("pack-{pack_name}.pack"));
        let index_path = pack_dir.join(format!("pack-{pack_name}.idx"));
        fs::write(&pack_path, &written.pack)?;
        fs::write(&index_path, &written.index)?;
        let verify = run_git_owned(
            &root,
            &[
                "verify-pack".into(),
                "-v".into(),
                index_path.to_string_lossy().into_owned(),
            ],
            &[],
        )?;
        let delta_entries = String::from_utf8_lossy(&verify)
            .lines()
            .filter(|line| {
                let fields = line.split_whitespace().count();
                fields >= 7 && line.contains(" blob ")
            })
            .count();
        if delta_entries == 0 {
            return Err(GitError::InvalidFormat(
                "rust-written pack did not contain a deltified blob".into(),
            ));
        }
        let upstream_body = run_git_owned(
            &root,
            &["cat-file".into(), "-p".into(), changed_oid.to_hex()],
            &[],
        )?;
        if upstream_body != changed_body {
            return Err(GitError::Command(
                "upstream git read different body from rust-written deltified pack".into(),
            ));
        }
        Ok(DeltaPackWriteParity {
            format,
            pack_name,
            base_oid: base_oid.to_hex(),
            changed_oid: changed_oid.to_hex(),
            delta_entries,
            upstream_body,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn sha256_loose_object_interop_parity() -> Result<LooseObjectInteropParity> {
    let root = unique_temp_dir("sley-sha256-loose");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<LooseObjectInteropParity> {
        run_git(&root, ["init", "-q", "--object-format=sha256"], &[])?;
        let body = b"hello from sha256 loose object\n";
        let object = EncodedObject::new(ObjectType::Blob, body.to_vec());
        let store = LooseObjectStore::from_git_dir(root.join(".git"), ObjectFormat::Sha256);
        let oid = store.write_object(object)?;
        let upstream_type = String::from_utf8_lossy(&run_git_owned(
            &root,
            &["cat-file".into(), "-t".into(), oid.to_hex()],
            &[],
        )?)
        .trim()
        .to_string();
        let upstream_body =
            run_git_owned(&root, &["cat-file".into(), "-p".into(), oid.to_hex()], &[])?;
        if upstream_type != "blob" || upstream_body != body {
            return Err(GitError::Command(
                "upstream git did not read rust-written sha256 loose object".into(),
            ));
        }
        Ok(LooseObjectInteropParity {
            oid: oid.to_hex(),
            format: ObjectFormat::Sha256,
            upstream_type,
            upstream_body,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn packed_odb_read_interop_parity() -> Result<PackedOdbInteropParity> {
    packed_odb_read_interop_parity_for_format(ObjectFormat::Sha1)
}

pub fn packed_odb_read_interop_parity_sha256() -> Result<PackedOdbInteropParity> {
    packed_odb_read_interop_parity_for_format(ObjectFormat::Sha256)
}

fn packed_odb_read_interop_parity_for_format(
    format: ObjectFormat,
) -> Result<PackedOdbInteropParity> {
    let root = unique_temp_dir("sley-packed-odb");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<PackedOdbInteropParity> {
        init_repo_for_format(&root, format)?;
        let body = b"hello from upstream pack\n";
        let oid = run_git(&root, ["hash-object", "-w", "--stdin"], body)?;
        let pack_hash = run_git(&root, ["pack-objects", ".git/objects/pack/pack"], &oid)?;
        let oid = String::from_utf8_lossy(&oid).trim().to_string();
        let pack_hash = String::from_utf8_lossy(&pack_hash).trim().to_string();
        let loose_path = root
            .join(".git")
            .join("objects")
            .join(&oid[..2])
            .join(&oid[2..]);
        fs::remove_file(loose_path)?;
        let pack_path = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join(format!("pack-{pack_hash}.pack"));
        let index_path = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join(format!("pack-{pack_hash}.idx"));
        if !pack_path.exists() || !index_path.exists() {
            return Err(GitError::not_found(
                "upstream git did not write expected pack/index",
            ));
        }
        let db = FileObjectDatabase::from_git_dir(root.join(".git"), format);
        let oid = ObjectId::from_hex(format, &oid)?;
        let object = db.read_object(&oid)?;
        if object.body != body {
            return Err(GitError::InvalidObject(
                "packed ODB reader returned different body".into(),
            ));
        }
        Ok(PackedOdbInteropParity {
            oid: oid.to_hex(),
            format,
            body: object.body.clone(),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn delta_packed_odb_read_interop_parity() -> Result<DeltaPackReadParity> {
    delta_packed_odb_read_interop_parity_for_format(ObjectFormat::Sha1)
}

pub fn delta_packed_odb_read_interop_parity_sha256() -> Result<DeltaPackReadParity> {
    delta_packed_odb_read_interop_parity_for_format(ObjectFormat::Sha256)
}

fn delta_packed_odb_read_interop_parity_for_format(
    format: ObjectFormat,
) -> Result<DeltaPackReadParity> {
    let root = unique_temp_dir("sley-packed-odb-delta");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<DeltaPackReadParity> {
        init_repo_for_format(&root, format)?;
        let base_body = repeated_blob_body("common payload\n", "base payload\n");
        let changed_body = repeated_blob_body("common payload\n", "changed payload\n");
        let base_oid = run_git(&root, ["hash-object", "-w", "--stdin"], &base_body)?;
        let changed_oid = run_git(&root, ["hash-object", "-w", "--stdin"], &changed_body)?;
        let oid_input = [base_oid.as_slice(), changed_oid.as_slice()].concat();
        let pack_hash = run_git(
            &root,
            [
                "pack-objects",
                "--window=10",
                "--depth=50",
                ".git/objects/pack/pack",
            ],
            &oid_input,
        )?;
        let pack_hash = String::from_utf8_lossy(&pack_hash).trim().to_string();
        let index_path = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join(format!("pack-{pack_hash}.idx"));
        let verify = run_git_owned(
            &root,
            &[
                "verify-pack".into(),
                "-v".into(),
                index_path.to_string_lossy().into_owned(),
            ],
            &[],
        )?;
        let delta_entries = String::from_utf8_lossy(&verify)
            .lines()
            .filter(|line| {
                let fields = line.split_whitespace().count();
                fields >= 7 && line.contains(" blob ")
            })
            .count();
        if delta_entries == 0 {
            return Err(GitError::InvalidFormat(
                "upstream pack did not contain a deltified blob".into(),
            ));
        }
        let base_oid = String::from_utf8_lossy(&base_oid).trim().to_string();
        let changed_oid = String::from_utf8_lossy(&changed_oid).trim().to_string();
        remove_loose_object(&root, &base_oid)?;
        remove_loose_object(&root, &changed_oid)?;

        let db = FileObjectDatabase::from_git_dir(root.join(".git"), format);
        let base_object = db.read_object(&ObjectId::from_hex(format, &base_oid)?)?;
        let changed_object = db.read_object(&ObjectId::from_hex(format, &changed_oid)?)?;
        if base_object.body != base_body || changed_object.body != changed_body {
            return Err(GitError::InvalidObject(
                "packed ODB reader did not reconstruct expected deltified blob bodies".into(),
            ));
        }
        Ok(DeltaPackReadParity {
            format,
            entries: 2,
            delta_entries,
            base_oid,
            changed_oid,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn remove_loose_object(root: &Path, oid: &str) -> Result<()> {
    fs::remove_file(
        root.join(".git")
            .join("objects")
            .join(&oid[..2])
            .join(&oid[2..]),
    )?;
    Ok(())
}

pub fn repository_config_interop_parity() -> Result<ConfigInteropParity> {
    let root = unique_temp_dir("sley-config-interop");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<ConfigInteropParity> {
        run_git(&root, ["init", "-q", "--object-format=sha256"], &[])?;
        let config = GitConfig::read(root.join(".git").join("config"))?;
        Ok(ConfigInteropParity {
            object_format: config.repository_object_format()?,
            bare: config.get_bool("core", None, "bare"),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn ls_tree_parity() -> Result<LsTreeParity> {
    ls_tree_parity_for_format(ObjectFormat::Sha1)
}

pub fn ls_tree_parity_sha256() -> Result<LsTreeParity> {
    ls_tree_parity_for_format(ObjectFormat::Sha256)
}

pub fn ls_tree_parity_for_format(format: ObjectFormat) -> Result<LsTreeParity> {
    let root = unique_temp_dir("sley-ls-tree");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<LsTreeParity> {
        init_repo_with_format(&root, format)?;
        run_git(&root, ["config", "user.name", "Example User"], &[])?;
        run_git(
            &root,
            ["config", "user.email", "example@example.invalid"],
            &[],
        )?;
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        fs::write(root.join("src").join("lib.rs"), b"pub fn hello() {}\n")?;
        run_git(&root, ["add", "hello.txt", "src/lib.rs"], &[])?;
        let tree_oid = String::from_utf8_lossy(&run_git(&root, ["write-tree"], &[])?)
            .trim()
            .to_string();
        run_git(
            &root,
            [
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "initial subject",
            ],
            &[],
        )?;
        run_git(
            &root,
            [
                "-c",
                "commit.gpgsign=false",
                "tag",
                "-a",
                "v1.0",
                "-m",
                "release",
            ],
            &[],
        )?;
        let db = FileObjectDatabase::from_git_dir(root.join(".git"), format);
        let revs = [tree_oid.as_str(), "HEAD", "refs/tags/v1.0"];
        let mut upstream = String::new();
        let mut rust = String::new();
        let mut name_only_upstream = String::new();
        let mut name_only_rust = String::new();
        let mut object_only_upstream = String::new();
        let mut object_only_rust = String::new();
        let mut long_upstream = String::new();
        let mut long_rust = String::new();
        let mut recursive_upstream = String::new();
        let mut recursive_rust = String::new();
        let mut recursive_object_only_upstream = String::new();
        let mut recursive_object_only_rust = String::new();
        let mut recursive_long_upstream = String::new();
        let mut recursive_long_rust = String::new();
        let mut recursive_name_only_upstream = String::new();
        let mut recursive_name_only_rust = String::new();
        let mut z_upstream = Vec::new();
        let mut z_rust = Vec::new();
        let mut name_only_z_upstream = Vec::new();
        let mut name_only_z_rust = Vec::new();
        let mut object_only_z_upstream = Vec::new();
        let mut object_only_z_rust = Vec::new();
        let mut long_z_upstream = Vec::new();
        let mut long_z_rust = Vec::new();
        let mut recursive_z_upstream = Vec::new();
        let mut recursive_z_rust = Vec::new();
        let mut recursive_object_only_z_upstream = Vec::new();
        let mut recursive_object_only_z_rust = Vec::new();
        let mut recursive_long_z_upstream = Vec::new();
        let mut recursive_long_z_rust = Vec::new();
        let mut recursive_name_only_z_upstream = Vec::new();
        let mut recursive_name_only_z_rust = Vec::new();
        for rev in revs {
            upstream.push_str(&String::from_utf8_lossy(&run_git(
                &root,
                ["ls-tree", rev],
                &[],
            )?));
            name_only_upstream.push_str(&String::from_utf8_lossy(&run_git(
                &root,
                ["ls-tree", "--name-only", rev],
                &[],
            )?));
            object_only_upstream.push_str(&String::from_utf8_lossy(&run_git(
                &root,
                ["ls-tree", "--object-only", rev],
                &[],
            )?));
            long_upstream.push_str(&String::from_utf8_lossy(&run_git(
                &root,
                ["ls-tree", "--long", rev],
                &[],
            )?));
            recursive_upstream.push_str(&String::from_utf8_lossy(&run_git(
                &root,
                ["ls-tree", "-r", rev],
                &[],
            )?));
            recursive_object_only_upstream.push_str(&String::from_utf8_lossy(&run_git(
                &root,
                ["ls-tree", "-r", "--object-only", rev],
                &[],
            )?));
            recursive_long_upstream.push_str(&String::from_utf8_lossy(&run_git(
                &root,
                ["ls-tree", "-r", "--long", rev],
                &[],
            )?));
            recursive_name_only_upstream.push_str(&String::from_utf8_lossy(&run_git(
                &root,
                ["ls-tree", "-r", "--name-only", rev],
                &[],
            )?));
            z_upstream.extend_from_slice(&run_git(&root, ["ls-tree", "-z", rev], &[])?);
            name_only_z_upstream.extend_from_slice(&run_git(
                &root,
                ["ls-tree", "--name-only", "-z", rev],
                &[],
            )?);
            object_only_z_upstream.extend_from_slice(&run_git(
                &root,
                ["ls-tree", "--object-only", "-z", rev],
                &[],
            )?);
            long_z_upstream.extend_from_slice(&run_git(
                &root,
                ["ls-tree", "--long", "-z", rev],
                &[],
            )?);
            recursive_z_upstream.extend_from_slice(&run_git(
                &root,
                ["ls-tree", "-r", "-z", rev],
                &[],
            )?);
            recursive_object_only_z_upstream.extend_from_slice(&run_git(
                &root,
                ["ls-tree", "-r", "--object-only", "-z", rev],
                &[],
            )?);
            recursive_long_z_upstream.extend_from_slice(&run_git(
                &root,
                ["ls-tree", "-r", "--long", "-z", rev],
                &[],
            )?);
            recursive_name_only_z_upstream.extend_from_slice(&run_git(
                &root,
                ["ls-tree", "-r", "--name-only", "-z", rev],
                &[],
            )?);
            let oid = sley_rev::resolve_revision(root.join(".git"), format, rev)?;
            let tree_oid = sley_rev::peel_to_tree(&db, format, &oid)?;
            let tree_object = db.read_object(&tree_oid)?;
            if tree_object.object_type != ObjectType::Tree {
                return Err(GitError::InvalidObject(
                    "tree-ish did not produce a tree".into(),
                ));
            }
            rust.push_str(&format_tree_entries(format, &tree_object.body)?);
            name_only_rust.push_str(&format_tree_names(format, &tree_object.body)?);
            object_only_rust.push_str(&String::from_utf8_lossy(&format_tree_object_ids(
                format,
                &tree_object.body,
                b'\n',
            )?));
            long_rust.push_str(&String::from_utf8_lossy(&format_tree_entries_long(
                &db,
                format,
                &tree_object.body,
                b'\n',
            )?));
            recursive_rust.push_str(&format_tree_entries_recursive(
                &db,
                format,
                &tree_object.body,
                "",
                false,
            )?);
            recursive_object_only_rust.push_str(&String::from_utf8_lossy(
                &format_tree_object_ids_recursive(&db, format, &tree_object.body, "", b'\n')?,
            ));
            recursive_long_rust.push_str(&String::from_utf8_lossy(
                &format_tree_entries_recursive_long(&db, format, &tree_object.body, "", b'\n')?,
            ));
            recursive_name_only_rust.push_str(&format_tree_entries_recursive(
                &db,
                format,
                &tree_object.body,
                "",
                true,
            )?);
            z_rust.extend_from_slice(&format_tree_entries_z(format, &tree_object.body)?);
            name_only_z_rust.extend_from_slice(&format_tree_names_z(format, &tree_object.body)?);
            object_only_z_rust.extend_from_slice(&format_tree_object_ids(
                format,
                &tree_object.body,
                0,
            )?);
            long_z_rust.extend_from_slice(&format_tree_entries_long(
                &db,
                format,
                &tree_object.body,
                0,
            )?);
            recursive_z_rust.extend_from_slice(&format_tree_entries_recursive_z(
                &db,
                format,
                &tree_object.body,
                "",
                false,
            )?);
            recursive_object_only_z_rust.extend_from_slice(&format_tree_object_ids_recursive(
                &db,
                format,
                &tree_object.body,
                "",
                0,
            )?);
            recursive_long_z_rust.extend_from_slice(&format_tree_entries_recursive_long(
                &db,
                format,
                &tree_object.body,
                "",
                0,
            )?);
            recursive_name_only_z_rust.extend_from_slice(&format_tree_entries_recursive_z(
                &db,
                format,
                &tree_object.body,
                "",
                true,
            )?);
        }
        if rust != upstream
            || name_only_rust != name_only_upstream
            || object_only_rust != object_only_upstream
            || long_rust != long_upstream
            || recursive_rust != recursive_upstream
            || recursive_object_only_rust != recursive_object_only_upstream
            || recursive_long_rust != recursive_long_upstream
            || recursive_name_only_rust != recursive_name_only_upstream
            || z_rust != z_upstream
            || name_only_z_rust != name_only_z_upstream
            || object_only_z_rust != object_only_z_upstream
            || long_z_rust != long_z_upstream
            || recursive_z_rust != recursive_z_upstream
            || recursive_object_only_z_rust != recursive_object_only_z_upstream
            || recursive_long_z_rust != recursive_long_z_upstream
            || recursive_name_only_z_rust != recursive_name_only_z_upstream
        {
            return Err(GitError::Command(format!(
                "ls-tree mismatch: expected {upstream:?}, got {rust:?}; name-only expected {name_only_upstream:?}, got {name_only_rust:?}; object-only expected {object_only_upstream:?}, got {object_only_rust:?}; long expected {long_upstream:?}, got {long_rust:?}; recursive expected {recursive_upstream:?}, got {recursive_rust:?}; recursive object-only expected {recursive_object_only_upstream:?}, got {recursive_object_only_rust:?}; recursive long expected {recursive_long_upstream:?}, got {recursive_long_rust:?}; recursive name-only expected {recursive_name_only_upstream:?}, got {recursive_name_only_rust:?}; -z expected {z_upstream:?}, got {z_rust:?}; name-only -z expected {name_only_z_upstream:?}, got {name_only_z_rust:?}; object-only -z expected {object_only_z_upstream:?}, got {object_only_z_rust:?}; long -z expected {long_z_upstream:?}, got {long_z_rust:?}; recursive -z expected {recursive_z_upstream:?}, got {recursive_z_rust:?}; recursive object-only -z expected {recursive_object_only_z_upstream:?}, got {recursive_object_only_z_rust:?}; recursive long -z expected {recursive_long_z_upstream:?}, got {recursive_long_z_rust:?}; recursive name-only -z expected {recursive_name_only_z_upstream:?}, got {recursive_name_only_z_rust:?}"
            )));
        }
        Ok(LsTreeParity {
            format,
            tree_oid,
            upstream,
            rust,
            name_only_upstream,
            name_only_rust,
            object_only_upstream,
            object_only_rust,
            long_upstream,
            long_rust,
            recursive_upstream,
            recursive_rust,
            recursive_object_only_upstream,
            recursive_object_only_rust,
            recursive_long_upstream,
            recursive_long_rust,
            recursive_name_only_upstream,
            recursive_name_only_rust,
            z_upstream,
            z_rust,
            name_only_z_upstream,
            name_only_z_rust,
            object_only_z_upstream,
            object_only_z_rust,
            long_z_upstream,
            long_z_rust,
            recursive_z_upstream,
            recursive_z_rust,
            recursive_object_only_z_upstream,
            recursive_object_only_z_rust,
            recursive_long_z_upstream,
            recursive_long_z_rust,
            recursive_name_only_z_upstream,
            recursive_name_only_z_rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn format_tree_entries(format: ObjectFormat, body: &[u8]) -> Result<String> {
    let mut out = String::new();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        out.push_str(&format!(
            "{:06o} {} {}\t{}\n",
            entry.mode,
            tree_entry_object_type(entry.mode).as_str(),
            entry.oid,
            String::from_utf8_lossy(entry.name)
        ));
    }
    Ok(out)
}

fn format_tree_names(format: ObjectFormat, body: &[u8]) -> Result<String> {
    let mut out = String::new();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        out.push_str(&format!("{}\n", String::from_utf8_lossy(entry.name)));
    }
    Ok(out)
}

fn format_tree_entries_z(format: ObjectFormat, body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        write!(
            out,
            "{:06o} {} {}\t",
            entry.mode,
            tree_entry_object_type(entry.mode).as_str(),
            entry.oid
        )?;
        out.extend_from_slice(entry.name);
        out.push(0);
    }
    Ok(out)
}

fn format_tree_names_z(format: ObjectFormat, body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        out.extend_from_slice(entry.name);
        out.push(0);
    }
    Ok(out)
}

fn format_tree_object_ids(format: ObjectFormat, body: &[u8], terminator: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        write!(out, "{}", entry.oid)?;
        out.push(terminator);
    }
    Ok(out)
}

fn format_tree_entries_long(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    body: &[u8],
    terminator: u8,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        let object_type = tree_entry_object_type(entry.mode);
        let size = tree_entry_size_field(db, object_type, &entry.oid)?;
        write!(
            out,
            "{:06o} {} {} {size:>7}\t",
            entry.mode,
            object_type.as_str(),
            entry.oid
        )?;
        out.extend_from_slice(entry.name);
        out.push(terminator);
    }
    Ok(out)
}

fn tree_entry_size_field(
    db: &FileObjectDatabase,
    object_type: ObjectType,
    oid: &ObjectId,
) -> Result<String> {
    if object_type != ObjectType::Blob {
        return Ok("-".into());
    }
    Ok(db.read_object(oid)?.body.len().to_string())
}

fn format_tree_entries_recursive(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    body: &[u8],
    prefix: &str,
    name_only: bool,
) -> Result<String> {
    let mut out = String::new();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        let name = String::from_utf8_lossy(entry.name);
        let path = format!("{prefix}{name}");
        if entry.mode == 0o040000 {
            let object = db.read_object(&entry.oid)?;
            if object.object_type != ObjectType::Tree {
                return Err(GitError::InvalidObject(
                    "recursive ls-tree entry was not a tree".into(),
                ));
            }
            out.push_str(&format_tree_entries_recursive(
                db,
                format,
                &object.body,
                &format!("{path}/"),
                name_only,
            )?);
        } else if name_only {
            out.push_str(&format!("{path}\n"));
        } else {
            out.push_str(&format!(
                "{:06o} {} {}\t{}\n",
                entry.mode,
                tree_entry_object_type(entry.mode).as_str(),
                entry.oid,
                path
            ));
        }
    }
    Ok(out)
}

fn format_tree_entries_recursive_long(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    body: &[u8],
    prefix: &str,
    terminator: u8,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        let name = String::from_utf8_lossy(entry.name);
        let path = format!("{prefix}{name}");
        if entry.mode == 0o040000 {
            let object = db.read_object(&entry.oid)?;
            if object.object_type != ObjectType::Tree {
                return Err(GitError::InvalidObject(
                    "recursive ls-tree entry was not a tree".into(),
                ));
            }
            out.extend_from_slice(&format_tree_entries_recursive_long(
                db,
                format,
                &object.body,
                &format!("{path}/"),
                terminator,
            )?);
        } else {
            let object_type = tree_entry_object_type(entry.mode);
            let size = tree_entry_size_field(db, object_type, &entry.oid)?;
            write!(
                out,
                "{:06o} {} {} {size:>7}\t{}",
                entry.mode,
                object_type.as_str(),
                entry.oid,
                path
            )?;
            out.push(terminator);
        }
    }
    Ok(out)
}

fn format_tree_object_ids_recursive(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    body: &[u8],
    prefix: &str,
    terminator: u8,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        if entry.mode == 0o040000 {
            let object = db.read_object(&entry.oid)?;
            if object.object_type != ObjectType::Tree {
                return Err(GitError::InvalidObject(
                    "recursive ls-tree entry was not a tree".into(),
                ));
            }
            let name = String::from_utf8_lossy(entry.name);
            out.extend_from_slice(&format_tree_object_ids_recursive(
                db,
                format,
                &object.body,
                &format!("{prefix}{name}/"),
                terminator,
            )?);
        } else {
            write!(out, "{}", entry.oid)?;
            out.push(terminator);
        }
    }
    Ok(out)
}

fn format_tree_entries_recursive_z(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    body: &[u8],
    prefix: &str,
    name_only: bool,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        let name = String::from_utf8_lossy(entry.name);
        let path = format!("{prefix}{name}");
        if entry.mode == 0o040000 {
            let object = db.read_object(&entry.oid)?;
            if object.object_type != ObjectType::Tree {
                return Err(GitError::InvalidObject(
                    "recursive ls-tree entry was not a tree".into(),
                ));
            }
            out.extend_from_slice(&format_tree_entries_recursive_z(
                db,
                format,
                &object.body,
                &format!("{path}/"),
                name_only,
            )?);
        } else if name_only {
            out.extend_from_slice(path.as_bytes());
            out.push(0);
        } else {
            write!(
                out,
                "{:06o} {} {}\t{}",
                entry.mode,
                tree_entry_object_type(entry.mode).as_str(),
                entry.oid,
                path
            )?;
            out.push(0);
        }
    }
    Ok(out)
}

pub fn log_parity() -> Result<LogParity> {
    log_parity_for_format(ObjectFormat::Sha1)
}

pub fn log_parity_sha256() -> Result<LogParity> {
    log_parity_for_format(ObjectFormat::Sha256)
}

pub fn log_parity_for_format(format: ObjectFormat) -> Result<LogParity> {
    let root = unique_temp_dir("sley-log");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<LogParity> {
        init_repo_with_format(&root, format)?;
        run_git(&root, ["config", "user.name", "Example User"], &[])?;
        run_git(
            &root,
            ["config", "user.email", "example@example.invalid"],
            &[],
        )?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        run_git(&root, ["add", "hello.txt"], &[])?;
        run_git(
            &root,
            [
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "initial subject",
            ],
            &[],
        )?;
        let commit_oid = String::from_utf8_lossy(&run_git(&root, ["rev-parse", "HEAD"], &[])?)
            .trim()
            .to_string();
        run_git(
            &root,
            [
                "-c",
                "commit.gpgsign=false",
                "tag",
                "-a",
                "v1.0",
                "-m",
                "release",
            ],
            &[],
        )?;
        let db = FileObjectDatabase::from_git_dir(root.join(".git"), format);
        let revs = ["HEAD", "refs/tags/v1.0"];
        let mut upstream = String::new();
        let mut rust = String::new();
        for (idx, rev) in revs.iter().enumerate() {
            if idx != 0 {
                upstream.push('\n');
                rust.push('\n');
            }
            upstream.push_str(
                String::from_utf8_lossy(&run_git(
                    &root,
                    [
                        "log",
                        "-1",
                        "--format=commit %H%nAuthor: %an <%ae>%n%n    %s",
                        rev,
                    ],
                    &[],
                )?)
                .trim_end_matches('\n'),
            );
            let oid = sley_rev::resolve_revision(root.join(".git"), format, rev)?;
            let commit = sley_rev::peel_to_commit(&db, format, &oid)?;
            rust.push_str(&format_log_record(&db, format, &commit)?);
        }
        if rust != upstream {
            return Err(GitError::Command(format!(
                "log mismatch: expected {upstream:?}, got {rust:?}"
            )));
        }
        Ok(LogParity {
            format,
            commit_oid,
            upstream,
            rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn format_log_record(
    reader: &impl ObjectReader,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<String> {
    let records = sley_rev::walk_commits(reader, format, [*oid])?;
    let record = records
        .first()
        .ok_or_else(|| GitError::not_found("commit record"))?;
    let mut out = String::new();
    out.push_str(&format!("commit {}\n", record.oid));
    let author = String::from_utf8_lossy(&record.commit.author);
    let author_identity = author
        .rsplit_once(' ')
        .and_then(|(left, _)| left.rsplit_once(' ').map(|(identity, _)| identity))
        .unwrap_or(&author);
    out.push_str(&format!("Author: {author_identity}\n\n"));
    if let Some(subject) = String::from_utf8_lossy(&record.commit.message)
        .lines()
        .next()
    {
        out.push_str(&format!("    {subject}"));
    }
    Ok(out)
}

pub fn cat_file_revision_parity() -> Result<CatFileRevisionParity> {
    cat_file_revision_parity_for_format(ObjectFormat::Sha1)
}

pub fn cat_file_revision_parity_sha256() -> Result<CatFileRevisionParity> {
    cat_file_revision_parity_for_format(ObjectFormat::Sha256)
}

pub fn cat_file_revision_parity_for_format(format: ObjectFormat) -> Result<CatFileRevisionParity> {
    let root = unique_temp_dir("sley-cat-file-rev");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<CatFileRevisionParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity.clone(),
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let mut db = FileObjectDatabase::from_git_dir(root.join(".git"), format);
        let tag_oid = sley_sequencer::create_annotated_tag(
            &mut db,
            sley_sequencer::TagCreate {
                object: commit.oid,
                object_type: ObjectType::Commit,
                name: b"v2.0".to_vec(),
                tagger: identity,
                message: b"release v2\n".to_vec(),
            },
        )?;
        FileRefStore::new(root.join(".git"), format).create_tag("v2.0", tag_oid)?;

        let revs = vec![
            "HEAD".to_string(),
            "refs/tags/v2.0".to_string(),
            commit.tree.to_hex(),
        ];
        let mut upstream = Vec::new();
        let mut rust = Vec::new();
        for rev in &revs {
            for mode in ["-e", "-t", "-s", "-p"] {
                upstream.push(run_git(&root, ["cat-file", mode, rev.as_str()], &[])?);
                rust.push(rust_cat_file(&root, format, mode, rev)?);
            }
        }
        if rust != upstream {
            return Err(GitError::Command(
                "cat-file revision output did not match upstream git".into(),
            ));
        }
        Ok(CatFileRevisionParity {
            format,
            revs,
            upstream,
            rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn rust_cat_file(root: &Path, format: ObjectFormat, mode: &str, rev: &str) -> Result<Vec<u8>> {
    let git_dir = root.join(".git");
    let oid = sley_rev::resolve_revision(&git_dir, format, rev)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let object = db.read_object(&oid)?;
    match mode {
        "-e" => Ok(Vec::new()),
        "-t" => Ok(format!("{}\n", object.object_type.as_str()).into_bytes()),
        "-s" => Ok(format!("{}\n", object.body.len()).into_bytes()),
        "-p" if object.object_type == ObjectType::Tree => {
            Ok(format_tree_entries(format, &object.body)?.into_bytes())
        }
        "-p" => Ok(object.body.clone()),
        _ => Err(GitError::Command(format!(
            "unsupported cat-file mode {mode}"
        ))),
    }
}

pub fn index_round_trip_parity() -> Result<IndexParity> {
    index_round_trip_parity_for_format(ObjectFormat::Sha1)
}

pub fn index_round_trip_parity_sha256() -> Result<IndexParity> {
    index_round_trip_parity_for_format(ObjectFormat::Sha256)
}

pub fn index_round_trip_parity_for_format(format: ObjectFormat) -> Result<IndexParity> {
    let root = unique_temp_dir("sley-index");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<IndexParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        run_git(&root, ["add", "hello.txt"], &[])?;
        let bytes = fs::read(root.join(".git").join("index"))?;
        let index = Index::parse(&bytes, format)?;
        let written = index.write(format)?;
        if written != bytes {
            return Err(GitError::InvalidFormat(
                "index did not round-trip byte-for-byte".into(),
            ));
        }
        Ok(IndexParity {
            format,
            entries: index.entries.len(),
            byte_len: bytes.len(),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn update_index_add_parity() -> Result<UpdateIndexParity> {
    update_index_add_parity_for_format(ObjectFormat::Sha1)
}

pub fn update_index_add_parity_sha256() -> Result<UpdateIndexParity> {
    update_index_add_parity_for_format(ObjectFormat::Sha256)
}

pub fn update_index_add_parity_for_format(format: ObjectFormat) -> Result<UpdateIndexParity> {
    let root = unique_temp_dir("sley-update-index");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<UpdateIndexParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        let update = sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let oid = update
            .updated
            .first()
            .ok_or_else(|| GitError::not_found("updated index object id"))?;
        let upstream =
            String::from_utf8_lossy(&run_git(&root, ["ls-files", "--stage"], &[])?).to_string();
        let expected = format!("100644 {} 0\thello.txt\n", oid);
        if upstream != expected {
            return Err(GitError::Command(format!(
                "ls-files mismatch: expected {expected:?}, got {upstream:?}"
            )));
        }
        Ok(UpdateIndexParity {
            format,
            upstream,
            expected,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn ls_files_stage_parity() -> Result<LsFilesStageParity> {
    ls_files_stage_parity_for_format(ObjectFormat::Sha1)
}

pub fn ls_files_stage_parity_sha256() -> Result<LsFilesStageParity> {
    ls_files_stage_parity_for_format(ObjectFormat::Sha256)
}

pub fn ls_files_stage_parity_for_format(format: ObjectFormat) -> Result<LsFilesStageParity> {
    let root = unique_temp_dir("sley-ls-files-stage");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<LsFilesStageParity> {
        init_repo_with_format(&root, format)?;
        fs::create_dir_all(root.join("src"))?;
        fs::create_dir_all(root.join("docs"))?;
        fs::write(root.join("README.md"), b"hello\n")?;
        fs::write(root.join("modified.txt"), b"before\n")?;
        fs::write(root.join("src").join("lib.rs"), b"pub fn hello() {}\n")?;
        fs::write(root.join("docs").join("notes.txt"), b"notes\n")?;
        fs::write(root.join("scratch.txt"), b"scratch\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[
                PathBuf::from("README.md"),
                PathBuf::from("modified.txt"),
                PathBuf::from("src/lib.rs"),
            ],
        )?;
        fs::write(root.join("modified.txt"), b"after\n")?;
        fs::remove_file(root.join("src").join("lib.rs"))?;
        let upstream =
            String::from_utf8_lossy(&run_git(&root, ["ls-files", "--stage"], &[])?).to_string();
        let index = Index::parse(&fs::read(root.join(".git").join("index"))?, format)?;
        let rust = String::from_utf8_lossy(&format_ls_files(&index, true, b'\n')).to_string();
        let upstream_z = run_git(&root, ["ls-files", "-z"], &[])?;
        let rust_z = format_ls_files(&index, false, 0);
        let upstream_stage_z = run_git(&root, ["ls-files", "--stage", "-z"], &[])?;
        let rust_stage_z = format_ls_files(&index, true, 0);
        let upstream_cached =
            String::from_utf8_lossy(&run_git(&root, ["ls-files", "--cached"], &[])?).to_string();
        let rust_cached =
            String::from_utf8_lossy(&format_ls_files(&index, false, b'\n')).to_string();
        let upstream_cached_z = run_git(&root, ["ls-files", "--cached", "-z"], &[])?;
        let rust_cached_z = format_ls_files(&index, false, 0);
        let upstream_others =
            String::from_utf8_lossy(&run_git(&root, ["ls-files", "--others"], &[])?).to_string();
        let other_paths = sley_worktree::untracked_paths(&root, root.join(".git"), format)?;
        let rust_others =
            String::from_utf8_lossy(&format_ls_file_paths(&other_paths, b'\n')).to_string();
        let upstream_others_z = run_git(&root, ["ls-files", "--others", "-z"], &[])?;
        let rust_others_z = format_ls_file_paths(&other_paths, 0);
        let upstream_stage_others =
            String::from_utf8_lossy(&run_git(&root, ["ls-files", "--stage", "--others"], &[])?)
                .to_string();
        let mut rust_stage_others_bytes = format_ls_file_paths(&other_paths, b'\n');
        rust_stage_others_bytes.extend_from_slice(&format_ls_files(&index, true, b'\n'));
        let rust_stage_others = String::from_utf8_lossy(&rust_stage_others_bytes).to_string();
        let upstream_cached_others =
            String::from_utf8_lossy(&run_git(&root, ["ls-files", "--cached", "--others"], &[])?)
                .to_string();
        let mut rust_cached_others_bytes = format_ls_file_paths(&other_paths, b'\n');
        rust_cached_others_bytes.extend_from_slice(&format_ls_files(&index, false, b'\n'));
        let rust_cached_others = String::from_utf8_lossy(&rust_cached_others_bytes).to_string();
        let upstream_deleted =
            String::from_utf8_lossy(&run_git(&root, ["ls-files", "--deleted"], &[])?).to_string();
        let deleted_entries =
            sley_worktree::deleted_index_entries(&root, root.join(".git"), format)?;
        let upstream_modified =
            String::from_utf8_lossy(&run_git(&root, ["ls-files", "--modified"], &[])?).to_string();
        let modified_entries =
            sley_worktree::modified_index_entries(&root, root.join(".git"), format)?;
        let rust_modified = String::from_utf8_lossy(&format_ls_files_from_entries(
            &modified_entries,
            false,
            b'\n',
        ))
        .to_string();
        let upstream_modified_z = run_git(&root, ["ls-files", "--modified", "-z"], &[])?;
        let rust_modified_z = format_ls_files_from_entries(&modified_entries, false, 0);
        let upstream_stage_modified =
            String::from_utf8_lossy(&run_git(&root, ["ls-files", "--stage", "--modified"], &[])?)
                .to_string();
        let rust_stage_modified_bytes =
            format_ls_files_stage_with_selected(&index.entries, &[], &modified_entries, b'\n');
        let rust_stage_modified = String::from_utf8_lossy(&rust_stage_modified_bytes).to_string();
        let rust_deleted = String::from_utf8_lossy(&format_ls_files_from_entries(
            &deleted_entries,
            false,
            b'\n',
        ))
        .to_string();
        let upstream_deleted_z = run_git(&root, ["ls-files", "--deleted", "-z"], &[])?;
        let rust_deleted_z = format_ls_files_from_entries(&deleted_entries, false, 0);
        let upstream_stage_deleted =
            String::from_utf8_lossy(&run_git(&root, ["ls-files", "--stage", "--deleted"], &[])?)
                .to_string();
        let rust_stage_deleted_bytes =
            format_ls_files_stage_with_selected(&index.entries, &deleted_entries, &[], b'\n');
        let rust_stage_deleted = String::from_utf8_lossy(&rust_stage_deleted_bytes).to_string();
        let upstream_others_deleted =
            String::from_utf8_lossy(&run_git(&root, ["ls-files", "--others", "--deleted"], &[])?)
                .to_string();
        let mut rust_others_deleted_bytes = format_ls_file_paths(&other_paths, b'\n');
        rust_others_deleted_bytes.extend_from_slice(&format_ls_files_from_entries(
            &deleted_entries,
            false,
            b'\n',
        ));
        let rust_others_deleted = String::from_utf8_lossy(&rust_others_deleted_bytes).to_string();
        let upstream_deleted_modified = String::from_utf8_lossy(&run_git(
            &root,
            ["ls-files", "--deleted", "--modified"],
            &[],
        )?)
        .to_string();
        let rust_deleted_modified_bytes = format_ls_files_selected(
            &index.entries,
            &deleted_entries,
            &modified_entries,
            false,
            false,
            false,
            b'\n',
        );
        let rust_deleted_modified =
            String::from_utf8_lossy(&rust_deleted_modified_bytes).to_string();
        let upstream_cached_modified = String::from_utf8_lossy(&run_git(
            &root,
            ["ls-files", "--cached", "--modified"],
            &[],
        )?)
        .to_string();
        let rust_cached_modified_bytes = format_ls_files_selected(
            &index.entries,
            &[],
            &modified_entries,
            true,
            false,
            false,
            b'\n',
        );
        let rust_cached_modified = String::from_utf8_lossy(&rust_cached_modified_bytes).to_string();
        let upstream_cached_deleted_modified = String::from_utf8_lossy(&run_git(
            &root,
            ["ls-files", "--cached", "--deleted", "--modified"],
            &[],
        )?)
        .to_string();
        let rust_cached_deleted_modified_bytes = format_ls_files_selected(
            &index.entries,
            &deleted_entries,
            &modified_entries,
            true,
            false,
            false,
            b'\n',
        );
        let rust_cached_deleted_modified =
            String::from_utf8_lossy(&rust_cached_deleted_modified_bytes).to_string();
        let upstream_deduplicate_deleted_modified = String::from_utf8_lossy(&run_git(
            &root,
            ["ls-files", "--deduplicate", "--deleted", "--modified"],
            &[],
        )?)
        .to_string();
        let rust_deduplicate_deleted_modified_bytes = format_ls_files_selected(
            &index.entries,
            &deleted_entries,
            &modified_entries,
            false,
            false,
            true,
            b'\n',
        );
        let rust_deduplicate_deleted_modified =
            String::from_utf8_lossy(&rust_deduplicate_deleted_modified_bytes).to_string();
        let upstream_deduplicate_cached_modified = String::from_utf8_lossy(&run_git(
            &root,
            ["ls-files", "--deduplicate", "--cached", "--modified"],
            &[],
        )?)
        .to_string();
        let rust_deduplicate_cached_modified_bytes = format_ls_files_selected(
            &index.entries,
            &[],
            &modified_entries,
            true,
            false,
            true,
            b'\n',
        );
        let rust_deduplicate_cached_modified =
            String::from_utf8_lossy(&rust_deduplicate_cached_modified_bytes).to_string();
        let upstream_stage_others_deleted = String::from_utf8_lossy(&run_git(
            &root,
            ["ls-files", "--stage", "--others", "--deleted"],
            &[],
        )?)
        .to_string();
        let mut rust_stage_others_deleted_bytes = format_ls_file_paths(&other_paths, b'\n');
        rust_stage_others_deleted_bytes.extend_from_slice(&format_ls_files_stage_with_selected(
            &index.entries,
            &deleted_entries,
            &[],
            b'\n',
        ));
        let rust_stage_others_deleted =
            String::from_utf8_lossy(&rust_stage_others_deleted_bytes).to_string();
        let upstream_stage_others_deleted_modified = String::from_utf8_lossy(&run_git(
            &root,
            ["ls-files", "--stage", "--others", "--deleted", "--modified"],
            &[],
        )?)
        .to_string();
        let mut rust_stage_others_deleted_modified_bytes =
            format_ls_file_paths(&other_paths, b'\n');
        rust_stage_others_deleted_modified_bytes.extend_from_slice(
            &format_ls_files_stage_with_selected(
                &index.entries,
                &deleted_entries,
                &modified_entries,
                b'\n',
            ),
        );
        let rust_stage_others_deleted_modified =
            String::from_utf8_lossy(&rust_stage_others_deleted_modified_bytes).to_string();
        if rust != upstream
            || rust_z != upstream_z
            || rust_stage_z != upstream_stage_z
            || rust_others != upstream_others
            || rust_others_z != upstream_others_z
            || rust_stage_others != upstream_stage_others
            || rust_deleted != upstream_deleted
            || rust_deleted_z != upstream_deleted_z
            || rust_stage_deleted != upstream_stage_deleted
            || rust_others_deleted != upstream_others_deleted
            || rust_stage_others_deleted != upstream_stage_others_deleted
            || rust_modified != upstream_modified
            || rust_modified_z != upstream_modified_z
            || rust_stage_modified != upstream_stage_modified
            || rust_deleted_modified != upstream_deleted_modified
            || rust_stage_others_deleted_modified != upstream_stage_others_deleted_modified
            || rust_cached != upstream_cached
            || rust_cached_z != upstream_cached_z
            || rust_cached_others != upstream_cached_others
            || rust_cached_modified != upstream_cached_modified
            || rust_cached_deleted_modified != upstream_cached_deleted_modified
            || rust_deduplicate_deleted_modified != upstream_deduplicate_deleted_modified
            || rust_deduplicate_cached_modified != upstream_deduplicate_cached_modified
        {
            return Err(GitError::Command(format!(
                "ls-files mismatch: stage expected {upstream:?}, got {rust:?}; -z expected {upstream_z:?}, got {rust_z:?}; --stage -z expected {upstream_stage_z:?}, got {rust_stage_z:?}; --cached expected {upstream_cached:?}, got {rust_cached:?}; --cached -z expected {upstream_cached_z:?}, got {rust_cached_z:?}; --others expected {upstream_others:?}, got {rust_others:?}; --others -z expected {upstream_others_z:?}, got {rust_others_z:?}; --stage --others expected {upstream_stage_others:?}, got {rust_stage_others:?}; --cached --others expected {upstream_cached_others:?}, got {rust_cached_others:?}; --deleted expected {upstream_deleted:?}, got {rust_deleted:?}; --deleted -z expected {upstream_deleted_z:?}, got {rust_deleted_z:?}; --stage --deleted expected {upstream_stage_deleted:?}, got {rust_stage_deleted:?}; --others --deleted expected {upstream_others_deleted:?}, got {rust_others_deleted:?}; --stage --others --deleted expected {upstream_stage_others_deleted:?}, got {rust_stage_others_deleted:?}; --modified expected {upstream_modified:?}, got {rust_modified:?}; --modified -z expected {upstream_modified_z:?}, got {rust_modified_z:?}; --stage --modified expected {upstream_stage_modified:?}, got {rust_stage_modified:?}; --deleted --modified expected {upstream_deleted_modified:?}, got {rust_deleted_modified:?}; --cached --modified expected {upstream_cached_modified:?}, got {rust_cached_modified:?}; --cached --deleted --modified expected {upstream_cached_deleted_modified:?}, got {rust_cached_deleted_modified:?}; --deduplicate --deleted --modified expected {upstream_deduplicate_deleted_modified:?}, got {rust_deduplicate_deleted_modified:?}; --deduplicate --cached --modified expected {upstream_deduplicate_cached_modified:?}, got {rust_deduplicate_cached_modified:?}; --stage --others --deleted --modified expected {upstream_stage_others_deleted_modified:?}, got {rust_stage_others_deleted_modified:?}"
            )));
        }
        Ok(LsFilesStageParity {
            format,
            upstream,
            rust,
            upstream_z,
            rust_z,
            upstream_stage_z,
            rust_stage_z,
            upstream_others,
            rust_others,
            upstream_others_z,
            rust_others_z,
            upstream_stage_others,
            rust_stage_others,
            upstream_deleted,
            rust_deleted,
            upstream_deleted_z,
            rust_deleted_z,
            upstream_stage_deleted,
            rust_stage_deleted,
            upstream_others_deleted,
            rust_others_deleted,
            upstream_stage_others_deleted,
            rust_stage_others_deleted,
            upstream_modified,
            rust_modified,
            upstream_modified_z,
            rust_modified_z,
            upstream_stage_modified,
            rust_stage_modified,
            upstream_deleted_modified,
            rust_deleted_modified,
            upstream_stage_others_deleted_modified,
            rust_stage_others_deleted_modified,
            upstream_cached,
            rust_cached,
            upstream_cached_z,
            rust_cached_z,
            upstream_cached_others,
            rust_cached_others,
            upstream_cached_modified,
            rust_cached_modified,
            upstream_cached_deleted_modified,
            rust_cached_deleted_modified,
            upstream_deduplicate_deleted_modified,
            rust_deduplicate_deleted_modified,
            upstream_deduplicate_cached_modified,
            rust_deduplicate_cached_modified,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn format_ls_files(index: &Index, stage: bool, terminator: u8) -> Vec<u8> {
    format_ls_files_from_entries(&index.entries, stage, terminator)
}

fn format_ls_files_from_entries(
    entries: &[sley_index::IndexEntry],
    stage: bool,
    terminator: u8,
) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        if stage {
            let stage = (entry.flags >> 12) & 0x3;
            out.extend_from_slice(format!("{:06o} {} {stage}\t", entry.mode, entry.oid).as_bytes());
        }
        out.extend_from_slice(entry.path.as_bytes());
        out.push(terminator);
    }
    out
}

fn format_ls_files_stage_with_selected(
    entries: &[sley_index::IndexEntry],
    deleted_entries: &[sley_index::IndexEntry],
    modified_entries: &[sley_index::IndexEntry],
    terminator: u8,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for entry in entries {
        out.extend_from_slice(&format_ls_files_selected_entry(
            entry,
            deleted_entries,
            modified_entries,
            LsFilesFormatOptions {
                cached: false,
                stage: true,
                deduplicate: false,
                terminator,
            },
            &mut seen,
        ));
        out.extend_from_slice(&format_ls_files_from_entries(
            std::slice::from_ref(entry),
            true,
            terminator,
        ));
    }
    out
}

fn format_ls_files_selected(
    entries: &[sley_index::IndexEntry],
    deleted_entries: &[sley_index::IndexEntry],
    modified_entries: &[sley_index::IndexEntry],
    cached: bool,
    stage: bool,
    deduplicate: bool,
    terminator: u8,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for entry in entries {
        out.extend_from_slice(&format_ls_files_selected_entry(
            entry,
            deleted_entries,
            modified_entries,
            LsFilesFormatOptions {
                cached,
                stage,
                deduplicate,
                terminator,
            },
            &mut seen,
        ));
    }
    out
}

fn format_ls_files_selected_entry(
    entry: &sley_index::IndexEntry,
    deleted_entries: &[sley_index::IndexEntry],
    modified_entries: &[sley_index::IndexEntry],
    options: LsFilesFormatOptions,
    seen: &mut Vec<Vec<u8>>,
) -> Vec<u8> {
    let mut out = Vec::new();
    if deleted_entries
        .iter()
        .any(|deleted_entry| deleted_entry.path == entry.path)
    {
        out.extend_from_slice(&format_ls_files_selected_single(entry, options, seen));
    }
    if modified_entries
        .iter()
        .any(|modified_entry| modified_entry.path == entry.path)
    {
        out.extend_from_slice(&format_ls_files_selected_single(entry, options, seen));
    }
    if options.cached {
        out.extend_from_slice(&format_ls_files_selected_single(entry, options, seen));
    }
    out
}

fn format_ls_files_selected_single(
    entry: &sley_index::IndexEntry,
    options: LsFilesFormatOptions,
    seen: &mut Vec<Vec<u8>>,
) -> Vec<u8> {
    if options.deduplicate && seen.iter().any(|p| p.as_slice() == entry.path.as_bytes()) {
        return Vec::new();
    }
    if options.deduplicate {
        seen.push(entry.path.as_bytes().to_vec());
    }
    format_ls_files_from_entries(
        std::slice::from_ref(entry),
        options.stage,
        options.terminator,
    )
}

#[derive(Clone, Copy)]
struct LsFilesFormatOptions {
    cached: bool,
    stage: bool,
    deduplicate: bool,
    terminator: u8,
}

fn format_ls_file_paths(paths: &[Vec<u8>], terminator: u8) -> Vec<u8> {
    let mut out = Vec::new();
    for path in paths {
        out.extend_from_slice(path);
        out.push(terminator);
    }
    out
}

pub fn update_ref_delete_parity() -> Result<UpdateRefDeleteParity> {
    update_ref_delete_parity_for_format(ObjectFormat::Sha1)
}

pub fn update_ref_delete_parity_sha256() -> Result<UpdateRefDeleteParity> {
    update_ref_delete_parity_for_format(ObjectFormat::Sha256)
}

pub fn update_ref_delete_parity_for_format(format: ObjectFormat) -> Result<UpdateRefDeleteParity> {
    let root = unique_temp_dir("sley-update-ref-delete");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<UpdateRefDeleteParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity.clone(),
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let store = FileRefStore::new(root.join(".git"), format);
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/topic".into(),
            expected: None,
            new: RefTarget::Direct(commit.oid),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(format)?,
                new_oid: commit.oid,
                committer: identity,
                message: b"update by test".to_vec(),
            }),
        });
        tx.commit()?;
        let before =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "--heads"], &[])?).to_string();
        let deleted = store.delete_ref("refs/heads/topic")?;
        let after =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "--heads"], &[])?).to_string();
        let expected_before = format!(
            "{} refs/heads/main\n{} refs/heads/topic\n",
            commit.oid, commit.oid
        );
        let expected_after = format!("{} refs/heads/main\n", commit.oid);
        if before != expected_before || after != expected_after || deleted.oid != commit.oid {
            return Err(GitError::Command(format!(
                "update-ref delete mismatch: before={before:?} after={after:?} deleted={}",
                deleted.oid
            )));
        }
        Ok(UpdateRefDeleteParity {
            format,
            before,
            after,
            deleted_oid: deleted.oid.to_hex(),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn update_ref_delete_packed_parity() -> Result<UpdateRefDeleteParity> {
    update_ref_delete_packed_parity_for_format(ObjectFormat::Sha1)
}

pub fn update_ref_delete_packed_parity_sha256() -> Result<UpdateRefDeleteParity> {
    update_ref_delete_packed_parity_for_format(ObjectFormat::Sha256)
}

pub fn update_ref_delete_packed_parity_for_format(
    format: ObjectFormat,
) -> Result<UpdateRefDeleteParity> {
    let root = unique_temp_dir("sley-update-ref-delete-packed");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<UpdateRefDeleteParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity,
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let store = FileRefStore::new(root.join(".git"), format);
        store.write_packed_refs(&[PackedRef {
            reference: Ref {
                name: "refs/heads/topic".into(),
                target: RefTarget::Direct(commit.oid),
            },
            peeled: None,
        }])?;
        let before =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "--heads"], &[])?).to_string();
        let deleted = store.delete_ref("refs/heads/topic")?;
        let after =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "--heads"], &[])?).to_string();
        let expected_before = format!(
            "{} refs/heads/main\n{} refs/heads/topic\n",
            commit.oid, commit.oid
        );
        let expected_after = format!("{} refs/heads/main\n", commit.oid);
        if before != expected_before || after != expected_after || deleted.oid != commit.oid {
            return Err(GitError::Command(format!(
                "packed update-ref delete mismatch: before={before:?} after={after:?} deleted={}",
                deleted.oid
            )));
        }
        Ok(UpdateRefDeleteParity {
            format,
            before,
            after,
            deleted_oid: deleted.oid.to_hex(),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn reflog_expire_parity() -> Result<ReflogExpireParity> {
    reflog_expire_parity_for_format(ObjectFormat::Sha1)
}

pub fn reflog_expire_parity_sha256() -> Result<ReflogExpireParity> {
    reflog_expire_parity_for_format(ObjectFormat::Sha256)
}

pub fn reflog_expire_parity_for_format(format: ObjectFormat) -> Result<ReflogExpireParity> {
    let root = unique_temp_dir("sley-reflog-expire");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<ReflogExpireParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"one\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity_old = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@60 +0000",
        )?;
        let _first = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity_old.clone(),
                committer: identity_old,
                message: b"first\n".to_vec(),
                reflog_message: b"commit: first".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;

        fs::write(root.join("hello.txt"), b"two\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity_new = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@100 +0000",
        )?;
        let _second = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity_new.clone(),
                committer: identity_new,
                message: b"second\n".to_vec(),
                reflog_message: b"commit: second".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;

        let before = String::from_utf8_lossy(&run_git(
            &root,
            ["reflog", "show", "--format=%gs", "refs/heads/main"],
            &[],
        )?)
        .to_string();
        let store = FileRefStore::new(root.join(".git"), format);
        let removed = store.expire_reflog_older_than("refs/heads/main", 80)?;
        let after = String::from_utf8_lossy(&run_git(
            &root,
            ["reflog", "show", "--format=%gs", "refs/heads/main"],
            &[],
        )?)
        .to_string();
        let remaining = store.read_reflog("refs/heads/main")?;
        if removed != 1
            || remaining.len() != 1
            || before != "commit: second\ncommit: first\n"
            || after != "commit: second\n"
        {
            return Err(GitError::Command(format!(
                "reflog expire mismatch: removed={removed} remaining={} before={before:?} after={after:?}",
                remaining.len()
            )));
        }

        Ok(ReflogExpireParity {
            format,
            before,
            after,
            removed,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn write_tree_parity() -> Result<WriteTreeParity> {
    write_tree_parity_for_format(ObjectFormat::Sha1)
}

pub fn write_tree_parity_sha256() -> Result<WriteTreeParity> {
    write_tree_parity_for_format(ObjectFormat::Sha256)
}

pub fn write_tree_parity_for_format(format: ObjectFormat) -> Result<WriteTreeParity> {
    let root = unique_temp_dir("sley-write-tree");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<WriteTreeParity> {
        init_repo_with_format(&root, format)?;
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("README.md"), b"readme\n")?;
        fs::write(root.join("src").join("lib.rs"), b"pub fn demo() {}\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("README.md"), PathBuf::from("src/lib.rs")],
        )?;
        let rust = sley_worktree::write_tree_from_index(root.join(".git"), format)?.to_hex();
        let upstream = String::from_utf8_lossy(&run_git(&root, ["write-tree"], &[])?)
            .trim()
            .to_string();
        if rust != upstream {
            return Err(GitError::Command(format!(
                "write-tree mismatch: expected {upstream}, got {rust}"
            )));
        }
        Ok(WriteTreeParity {
            format,
            upstream,
            rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn commit_tree_parity() -> Result<CommitTreeParity> {
    commit_tree_parity_for_format(ObjectFormat::Sha1)
}

pub fn commit_tree_parity_sha256() -> Result<CommitTreeParity> {
    commit_tree_parity_for_format(ObjectFormat::Sha256)
}

pub fn commit_tree_parity_for_format(format: ObjectFormat) -> Result<CommitTreeParity> {
    let root = unique_temp_dir("sley-commit-tree");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<CommitTreeParity> {
        init_repo_with_format(&root, format)?;
        let tree = String::from_utf8_lossy(&run_git(&root, ["mktree"], &[])?)
            .trim()
            .to_string();
        let env = [
            ("GIT_AUTHOR_NAME", "Example User"),
            ("GIT_AUTHOR_EMAIL", "example@example.invalid"),
            ("GIT_AUTHOR_DATE", "@0 +0000"),
            ("GIT_COMMITTER_NAME", "Example User"),
            ("GIT_COMMITTER_EMAIL", "example@example.invalid"),
            ("GIT_COMMITTER_DATE", "@0 +0000"),
        ];
        let upstream = String::from_utf8_lossy(&run_git_with_env(
            &root,
            ["commit-tree", tree.as_str(), "-m", "initial subject"],
            &[],
            env,
        )?)
        .trim()
        .to_string();
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let mut db = FileObjectDatabase::from_git_dir(root.join(".git"), format);
        let rust = sley_sequencer::create_commit(
            &mut db,
            sley_sequencer::CommitCreate {
                tree: ObjectId::from_hex(format, &tree)?,
                parents: Vec::new(),
                author: identity.clone(),
                committer: identity,
                message: b"initial subject\n".to_vec(),
                encoding: None,
            signature: None,
            },
        )?
        .to_hex();
        if rust != upstream {
            return Err(GitError::Command(format!(
                "commit-tree mismatch: expected {upstream}, got {rust}"
            )));
        }
        let body = run_git_owned(&root, &["cat-file".into(), "-p".into(), rust.clone()], &[])?;
        Ok(CommitTreeParity {
            format,
            upstream,
            rust,
            body,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn commit_index_parity() -> Result<CommitIndexParity> {
    commit_index_parity_for_format(ObjectFormat::Sha1)
}

pub fn commit_index_parity_sha256() -> Result<CommitIndexParity> {
    commit_index_parity_for_format(ObjectFormat::Sha256)
}

pub fn commit_index_parity_for_format(format: ObjectFormat) -> Result<CommitIndexParity> {
    let root = unique_temp_dir("sley-commit-index");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<CommitIndexParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let result = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity,
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let head = String::from_utf8_lossy(&run_git(&root, ["rev-parse", "HEAD"], &[])?)
            .trim()
            .to_string();
        if head != result.oid.to_hex() {
            return Err(GitError::Command(format!(
                "rev-parse HEAD mismatch: expected {}, got {head}",
                result.oid
            )));
        }
        let log = String::from_utf8_lossy(&run_git(
            &root,
            ["log", "--format=commit %H%nAuthor: %an <%ae>%n%n    %s"],
            &[],
        )?)
        .trim_end_matches('\n')
        .to_string();
        let expected_log = format!(
            "commit {}\nAuthor: Example User <example@example.invalid>\n\n    initial subject",
            result.oid
        );
        if log != expected_log {
            return Err(GitError::Command(format!(
                "log mismatch: expected {expected_log:?}, got {log:?}"
            )));
        }
        Ok(CommitIndexParity {
            format,
            head,
            updated_ref: result.updated_ref,
            log,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn add_status_parity() -> Result<AddStatusParity> {
    add_status_parity_for_format(ObjectFormat::Sha1)
}

pub fn add_status_parity_sha256() -> Result<AddStatusParity> {
    add_status_parity_for_format(ObjectFormat::Sha256)
}

pub fn add_status_parity_for_format(format: ObjectFormat) -> Result<AddStatusParity> {
    let root = unique_temp_dir("sley-add-status");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<AddStatusParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        fs::write(root.join("extra.txt"), b"extra\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let upstream =
            String::from_utf8_lossy(&run_git(&root, ["status", "--short"], &[])?).to_string();
        let porcelain_upstream =
            String::from_utf8_lossy(&run_git(&root, ["status", "--porcelain=v1"], &[])?)
                .to_string();
        let porcelain_branch_upstream = String::from_utf8_lossy(&run_git(
            &root,
            ["status", "--porcelain=v1", "--branch"],
            &[],
        )?)
        .to_string();
        let mut entries = Vec::new();
        sley_worktree::stream_short_status(&root, root.join(".git"), format, |entry| {
            entries.push(entry.to_owned_entry());
            Ok(sley_worktree::StreamControl::Continue)
        })?;
        let rust = entries
            .iter()
            .map(|entry| format!("{}\n", entry.line()))
            .collect::<String>();
        let porcelain_rust = rust.clone();
        let branch_header = status_branch_header(&root.join(".git"), format)?;
        let porcelain_branch_rust = format!("{branch_header}\n{rust}");
        let porcelain_z_upstream = run_git(&root, ["status", "--porcelain=v1", "-z"], &[])?;
        let porcelain_z_rust = format_short_status_z(&entries);
        let porcelain_branch_z_upstream =
            run_git(&root, ["status", "--porcelain=v1", "--branch", "-z"], &[])?;
        let porcelain_branch_z_rust = format_short_status_branch_z(&branch_header, &entries);
        if rust != upstream
            || porcelain_rust != porcelain_upstream
            || porcelain_branch_rust != porcelain_branch_upstream
            || porcelain_z_rust != porcelain_z_upstream
            || porcelain_branch_z_rust != porcelain_branch_z_upstream
        {
            return Err(GitError::Command(format!(
                "status mismatch: expected {upstream:?}, got {rust:?}; porcelain expected {porcelain_upstream:?}, got {porcelain_rust:?}; porcelain branch expected {porcelain_branch_upstream:?}, got {porcelain_branch_rust:?}; porcelain -z expected {porcelain_z_upstream:?}, got {porcelain_z_rust:?}; porcelain branch -z expected {porcelain_branch_z_upstream:?}, got {porcelain_branch_z_rust:?}"
            )));
        }
        Ok(AddStatusParity {
            format,
            upstream,
            rust,
            porcelain_upstream,
            porcelain_rust,
            porcelain_branch_upstream,
            porcelain_branch_rust,
            porcelain_z_upstream,
            porcelain_z_rust,
            porcelain_branch_z_upstream,
            porcelain_branch_z_rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn format_short_status_z(entries: &[sley_worktree::ShortStatusEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        out.push(entry.index);
        out.push(entry.worktree);
        out.push(b' ');
        out.extend_from_slice(&entry.path);
        out.push(0);
    }
    out
}

fn format_short_status_branch_z(
    branch_header: &str,
    entries: &[sley_worktree::ShortStatusEntry],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(branch_header.as_bytes());
    out.push(0);
    out.extend_from_slice(&format_short_status_z(entries));
    out
}

fn status_branch_header(git_dir: &Path, format: ObjectFormat) -> Result<String> {
    let store = FileRefStore::new(git_dir, format);
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            if let Some(branch) = target.strip_prefix("refs/heads/") {
                if store.read_ref(&target)?.is_some() {
                    Ok(format!("## {branch}"))
                } else {
                    Ok(format!("## No commits yet on {branch}"))
                }
            } else {
                Ok(format!("## {target}"))
            }
        }
        Some(RefTarget::Direct(_)) | None => Ok("## HEAD (no branch)".into()),
    }
}

pub fn branch_create_parity() -> Result<BranchParity> {
    branch_create_parity_for_format(ObjectFormat::Sha1)
}

pub fn branch_create_parity_sha256() -> Result<BranchParity> {
    branch_create_parity_for_format(ObjectFormat::Sha256)
}

pub fn branch_create_parity_for_format(format: ObjectFormat) -> Result<BranchParity> {
    let root = unique_temp_dir("sley-branch");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<BranchParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity.clone(),
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let store = FileRefStore::new(root.join(".git"), format);
        let commit_oid = commit.oid;
        let commit_hex = commit_oid.to_hex();
        store.create_branch(
            "feature",
            commit_oid,
            identity,
            b"branch: Created from main".to_vec(),
        )?;
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "refs/remotes/origin/main".into(),
            expected: None,
            new: RefTarget::Direct(commit_oid),
            reflog: None,
        });
        tx.commit()?;
        let upstream =
            String::from_utf8_lossy(&run_git(&root, ["branch", "--list"], &[])?).to_string();
        let expected = "  feature\n* main\n".to_string();
        let remotes_upstream =
            String::from_utf8_lossy(&run_git(&root, ["branch", "-r"], &[])?).to_string();
        let remotes_expected = "  origin/main\n".to_string();
        let all_upstream =
            String::from_utf8_lossy(&run_git(&root, ["branch", "-a"], &[])?).to_string();
        let all_expected = "  feature\n* main\n  remotes/origin/main\n".to_string();
        let points_at_upstream =
            String::from_utf8_lossy(&run_git(&root, ["branch", "--points-at", "HEAD"], &[])?)
                .to_string();
        let points_at_expected = expected.clone();
        let points_at_arg = format!("--points-at={commit_hex}");
        let points_at_oid_upstream =
            String::from_utf8_lossy(&run_git(&root, ["branch", points_at_arg.as_str()], &[])?)
                .to_string();
        let points_at_oid_expected = expected.clone();
        if upstream != expected
            || remotes_upstream != remotes_expected
            || all_upstream != all_expected
            || points_at_upstream != points_at_expected
            || points_at_oid_upstream != points_at_oid_expected
        {
            return Err(GitError::Command(format!(
                "branch list mismatch: expected {expected:?}, got {upstream:?}; -r expected {remotes_expected:?}, got {remotes_upstream:?}; -a expected {all_expected:?}, got {all_upstream:?}; --points-at expected {points_at_expected:?}, got {points_at_upstream:?}; --points-at=<oid> expected {points_at_oid_expected:?}, got {points_at_oid_upstream:?}"
            )));
        }
        Ok(BranchParity {
            format,
            upstream,
            expected,
            points_at_upstream,
            points_at_expected,
            points_at_oid_upstream,
            points_at_oid_expected,
            remotes_upstream,
            remotes_expected,
            all_upstream,
            all_expected,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn branch_show_current_parity() -> Result<BranchShowCurrentParity> {
    branch_show_current_parity_for_format(ObjectFormat::Sha1)
}

pub fn branch_show_current_parity_sha256() -> Result<BranchShowCurrentParity> {
    branch_show_current_parity_for_format(ObjectFormat::Sha256)
}

pub fn branch_show_current_parity_for_format(
    format: ObjectFormat,
) -> Result<BranchShowCurrentParity> {
    let root = unique_temp_dir("sley-branch-show-current");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<BranchShowCurrentParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity,
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let upstream = String::from_utf8_lossy(&run_git(&root, ["branch", "--show-current"], &[])?)
            .to_string();
        let rust = FileRefStore::new(root.join(".git"), format)
            .current_branch()?
            .map(|branch| format!("{branch}\n"))
            .unwrap_or_default();
        if rust != upstream {
            return Err(GitError::Command(format!(
                "branch --show-current mismatch: expected {upstream:?}, got {rust:?}"
            )));
        }
        Ok(BranchShowCurrentParity {
            format,
            upstream,
            rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn branch_delete_parity() -> Result<BranchDeleteParity> {
    branch_delete_parity_for_format(ObjectFormat::Sha1)
}

pub fn branch_delete_parity_sha256() -> Result<BranchDeleteParity> {
    branch_delete_parity_for_format(ObjectFormat::Sha256)
}

pub fn branch_delete_parity_for_format(format: ObjectFormat) -> Result<BranchDeleteParity> {
    let root = unique_temp_dir("sley-branch-delete");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<BranchDeleteParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity.clone(),
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let store = FileRefStore::new(root.join(".git"), format);
        store.create_branch(
            "feature",
            commit.oid,
            identity,
            b"branch: Created from main".to_vec(),
        )?;
        let before =
            String::from_utf8_lossy(&run_git(&root, ["branch", "--list"], &[])?).to_string();
        let deleted = store.delete_branch("feature")?;
        let after =
            String::from_utf8_lossy(&run_git(&root, ["branch", "--list"], &[])?).to_string();
        let expected_before = "  feature\n* main\n";
        let expected_after = "* main\n";
        if before != expected_before || after != expected_after || deleted.oid != commit.oid {
            return Err(GitError::Command(format!(
                "branch delete mismatch: before={before:?} after={after:?} deleted={}",
                deleted.oid
            )));
        }
        Ok(BranchDeleteParity {
            format,
            before,
            after,
            deleted_oid: deleted.oid.to_hex(),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn checkout_branch_parity() -> Result<CheckoutParity> {
    checkout_branch_parity_for_format(ObjectFormat::Sha1)
}

pub fn checkout_branch_parity_sha256() -> Result<CheckoutParity> {
    checkout_branch_parity_for_format(ObjectFormat::Sha256)
}

pub fn checkout_branch_parity_for_format(format: ObjectFormat) -> Result<CheckoutParity> {
    let root = unique_temp_dir("sley-checkout");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<CheckoutParity> {
        init_repo_with_format(&root, format)?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;

        fs::write(root.join("hello.txt"), b"feature\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let feature_commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity.clone(),
                message: b"feature subject\n".to_vec(),
                reflog_message: b"commit: feature subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let store = FileRefStore::new(root.join(".git"), format);
        store.create_branch(
            "feature",
            feature_commit.oid,
            identity.clone(),
            b"branch: Created from main".to_vec(),
        )?;

        fs::write(root.join("hello.txt"), b"main\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity.clone(),
                message: b"main subject\n".to_vec(),
                reflog_message: b"commit: main subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;

        sley_worktree::checkout_branch(&root, root.join(".git"), format, "feature", identity)?;
        let branch = String::from_utf8_lossy(&run_git(&root, ["branch", "--show-current"], &[])?)
            .trim()
            .to_string();
        let head = String::from_utf8_lossy(&run_git(&root, ["rev-parse", "HEAD"], &[])?)
            .trim()
            .to_string();
        let body = fs::read(root.join("hello.txt"))?;
        let status =
            String::from_utf8_lossy(&run_git(&root, ["status", "--short"], &[])?).to_string();
        if branch != "feature"
            || head != feature_commit.oid.to_hex()
            || body != b"feature\n"
            || !status.is_empty()
        {
            return Err(GitError::Command(format!(
                "checkout mismatch: branch={branch:?} head={head:?} body={body:?} status={status:?}"
            )));
        }
        Ok(CheckoutParity {
            format,
            branch,
            head,
            body,
            status,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn tag_create_parity() -> Result<TagParity> {
    tag_create_parity_for_format(ObjectFormat::Sha1)
}

pub fn tag_create_parity_sha256() -> Result<TagParity> {
    tag_create_parity_for_format(ObjectFormat::Sha256)
}

pub fn tag_create_parity_for_format(format: ObjectFormat) -> Result<TagParity> {
    let root = unique_temp_dir("sley-tag");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<TagParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity,
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let store = FileRefStore::new(root.join(".git"), format);
        store.create_tag("v1.0", commit.oid)?;
        let upstream =
            String::from_utf8_lossy(&run_git(&root, ["tag", "--list"], &[])?).to_string();
        let expected = "v1.0\n".to_string();
        if upstream != expected {
            return Err(GitError::Command(format!(
                "tag list mismatch: expected {expected:?}, got {upstream:?}"
            )));
        }
        let show_ref =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "--tags"], &[])?).to_string();
        let expected_show_ref = format!("{} refs/tags/v1.0\n", commit.oid);
        if show_ref != expected_show_ref {
            return Err(GitError::Command(format!(
                "show-ref --tags mismatch: expected {expected_show_ref:?}, got {show_ref:?}"
            )));
        }
        Ok(TagParity {
            format,
            upstream,
            expected,
            show_ref,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn tag_delete_parity() -> Result<TagDeleteParity> {
    tag_delete_parity_for_format(ObjectFormat::Sha1)
}

pub fn tag_delete_parity_sha256() -> Result<TagDeleteParity> {
    tag_delete_parity_for_format(ObjectFormat::Sha256)
}

pub fn tag_delete_parity_for_format(format: ObjectFormat) -> Result<TagDeleteParity> {
    let root = unique_temp_dir("sley-tag-delete");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<TagDeleteParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity,
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let store = FileRefStore::new(root.join(".git"), format);
        store.create_tag("v1.0", commit.oid)?;
        let before = String::from_utf8_lossy(&run_git(&root, ["tag", "--list"], &[])?).to_string();
        let deleted = store.delete_tag("v1.0")?;
        let after = String::from_utf8_lossy(&run_git(&root, ["tag", "--list"], &[])?).to_string();
        if before != "v1.0\n" || !after.is_empty() || deleted.oid != commit.oid {
            return Err(GitError::Command(format!(
                "tag delete mismatch: before={before:?} after={after:?} deleted={}",
                deleted.oid
            )));
        }
        Ok(TagDeleteParity {
            format,
            before,
            after,
            deleted_oid: deleted.oid.to_hex(),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn annotated_tag_create_parity() -> Result<AnnotatedTagParity> {
    annotated_tag_create_parity_for_format(ObjectFormat::Sha1)
}

pub fn annotated_tag_create_parity_sha256() -> Result<AnnotatedTagParity> {
    annotated_tag_create_parity_for_format(ObjectFormat::Sha256)
}

pub fn annotated_tag_create_parity_for_format(format: ObjectFormat) -> Result<AnnotatedTagParity> {
    let root = unique_temp_dir("sley-annotated-tag");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<AnnotatedTagParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity.clone(),
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let mut db = FileObjectDatabase::from_git_dir(root.join(".git"), format);
        let tag_oid = sley_sequencer::create_annotated_tag(
            &mut db,
            sley_sequencer::TagCreate {
                object: commit.oid,
                object_type: ObjectType::Commit,
                name: b"v2.0".to_vec(),
                tagger: identity,
                message: b"release v2\n".to_vec(),
            },
        )?;
        let store = FileRefStore::new(root.join(".git"), format);
        store.create_tag("v2.0", tag_oid)?;
        let upstream_type =
            String::from_utf8_lossy(&run_git(&root, ["cat-file", "-t", "refs/tags/v2.0"], &[])?)
                .trim()
                .to_string();
        if upstream_type != "tag" {
            return Err(GitError::Command(format!(
                "annotated tag type mismatch: got {upstream_type:?}"
            )));
        }
        let upstream_body = run_git(&root, ["cat-file", "-p", "refs/tags/v2.0"], &[])?;
        let expected_body = db.read_object(&tag_oid)?.body.clone();
        if upstream_body != expected_body {
            return Err(GitError::Command(format!(
                "annotated tag body mismatch: expected {:?}, got {:?}",
                String::from_utf8_lossy(&expected_body),
                String::from_utf8_lossy(&upstream_body)
            )));
        }
        let show_ref =
            String::from_utf8_lossy(&run_git(&root, ["show-ref", "--tags"], &[])?).to_string();
        let expected_show_ref = format!("{tag_oid} refs/tags/v2.0\n");
        if show_ref != expected_show_ref {
            return Err(GitError::Command(format!(
                "show-ref --tags mismatch: expected {expected_show_ref:?}, got {show_ref:?}"
            )));
        }
        Ok(AnnotatedTagParity {
            format,
            tag_oid: tag_oid.to_hex(),
            target_oid: commit.oid.to_hex(),
            upstream_type,
            upstream_body,
            expected_body,
            show_ref,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn diff_name_status_parity() -> Result<DiffNameStatusParity> {
    diff_name_status_parity_for_format(ObjectFormat::Sha1)
}

pub fn diff_name_status_parity_sha256() -> Result<DiffNameStatusParity> {
    diff_name_status_parity_for_format(ObjectFormat::Sha256)
}

pub fn diff_name_status_parity_for_format(format: ObjectFormat) -> Result<DiffNameStatusParity> {
    let root = unique_temp_dir("sley-diff-name-status");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<DiffNameStatusParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("add.txt"), b"base add\n")?;
        fs::write(root.join("delete.txt"), b"delete\n")?;
        fs::write(root.join("modify.txt"), b"before\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[
                PathBuf::from("add.txt"),
                PathBuf::from("delete.txt"),
                PathBuf::from("modify.txt"),
            ],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity,
                message: b"base\n".to_vec(),
                reflog_message: b"commit: base".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;

        fs::write(root.join("add.txt"), b"base add\n")?;
        fs::write(root.join("modify.txt"), b"after\n")?;
        fs::remove_file(root.join("delete.txt"))?;
        fs::write(root.join("new.txt"), b"new\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("new.txt")],
        )?;
        fs::write(root.join("untracked.txt"), b"ignored by diff\n")?;

        let upstream =
            String::from_utf8_lossy(&run_git(&root, ["diff", "--name-status", "HEAD"], &[])?)
                .to_string();
        let name_only_upstream =
            String::from_utf8_lossy(&run_git(&root, ["diff", "--name-only", "HEAD"], &[])?)
                .to_string();
        let cached_upstream = String::from_utf8_lossy(&run_git(
            &root,
            ["diff", "--cached", "--name-status", "HEAD"],
            &[],
        )?)
        .to_string();
        let cached_name_only_upstream = String::from_utf8_lossy(&run_git(
            &root,
            ["diff", "--cached", "--name-only", "HEAD"],
            &[],
        )?)
        .to_string();
        let rust =
            sley_diff_merge::diff_name_status_head_worktree(&root, root.join(".git"), format)?
                .into_iter()
                .map(|entry| format!("{}\n", entry.line()))
                .collect::<String>();
        let name_only_rust =
            sley_diff_merge::diff_name_status_head_worktree(&root, root.join(".git"), format)?
                .into_iter()
                .map(|entry| format!("{}\n", String::from_utf8_lossy(entry.path.as_bytes())))
                .collect::<String>();
        let cached_rust = sley_diff_merge::diff_name_status_head_index(root.join(".git"), format)?
            .into_iter()
            .map(|entry| format!("{}\n", entry.line()))
            .collect::<String>();
        let cached_name_only_rust =
            sley_diff_merge::diff_name_status_head_index(root.join(".git"), format)?
                .into_iter()
                .map(|entry| format!("{}\n", String::from_utf8_lossy(entry.path.as_bytes())))
                .collect::<String>();
        if rust != upstream
            || name_only_rust != name_only_upstream
            || cached_rust != cached_upstream
            || cached_name_only_rust != cached_name_only_upstream
        {
            return Err(GitError::Command(format!(
                "diff mismatch: --name-status expected {upstream:?}, got {rust:?}; --name-only expected {name_only_upstream:?}, got {name_only_rust:?}; --cached --name-status expected {cached_upstream:?}, got {cached_rust:?}; --cached --name-only expected {cached_name_only_upstream:?}, got {cached_name_only_rust:?}"
            )));
        }

        run_git(&root, ["reset", "--hard", "-q", "HEAD"], &[])?;
        run_git(&root, ["clean", "-fdq"], &[])?;
        fs::write(root.join("copy-source.txt"), b"copy me\n")?;
        fs::write(root.join("rename-old.txt"), b"rename me\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[
                PathBuf::from("copy-source.txt"),
                PathBuf::from("rename-old.txt"),
            ],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity,
                message: b"copy rename base\n".to_vec(),
                reflog_message: b"commit: copy rename base".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        fs::copy(root.join("copy-source.txt"), root.join("copy-dest.txt"))?;
        fs::rename(root.join("rename-old.txt"), root.join("rename-new.txt"))?;
        run_git(&root, ["add", "-A"], &[])?;

        let rename_copy_upstream = String::from_utf8_lossy(&run_git(
            &root,
            [
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-status",
                "HEAD",
            ],
            &[],
        )?)
        .to_string();
        let rename_copy_name_only_upstream = String::from_utf8_lossy(&run_git(
            &root,
            [
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-only",
                "HEAD",
            ],
            &[],
        )?)
        .to_string();
        let rename_copy_entries = sley_diff_merge::diff_name_status_head_index_with_options(
            root.join(".git"),
            format,
            sley_diff_merge::DiffNameStatusOptions {
                detect_renames: true,
                detect_copies: true,
                find_copies_harder: true,
                rename_empty: true,
            },
        )?;
        let rename_copy_rust = rename_copy_entries
            .iter()
            .map(|entry| format!("{}\n", entry.line()))
            .collect::<String>();
        let rename_copy_name_only_rust = rename_copy_entries
            .iter()
            .map(|entry| format!("{}\n", String::from_utf8_lossy(entry.path.as_bytes())))
            .collect::<String>();
        if rename_copy_rust != rename_copy_upstream
            || rename_copy_name_only_rust != rename_copy_name_only_upstream
        {
            return Err(GitError::Command(format!(
                "diff rename/copy mismatch: --cached -C --find-copies-harder --name-status expected {rename_copy_upstream:?}, got {rename_copy_rust:?}; --name-only expected {rename_copy_name_only_upstream:?}, got {rename_copy_name_only_rust:?}"
            )));
        }
        Ok(DiffNameStatusParity {
            format,
            upstream,
            rust,
            name_only_upstream,
            name_only_rust,
            cached_upstream,
            cached_rust,
            cached_name_only_upstream,
            cached_name_only_rust,
            rename_copy_upstream,
            rename_copy_rust,
            rename_copy_name_only_upstream,
            rename_copy_name_only_rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn rev_parse_parity() -> Result<RevParseParity> {
    rev_parse_parity_for_format(ObjectFormat::Sha1)
}

pub fn rev_parse_parity_sha256() -> Result<RevParseParity> {
    rev_parse_parity_for_format(ObjectFormat::Sha256)
}

pub fn rev_parse_parity_for_format(format: ObjectFormat) -> Result<RevParseParity> {
    let root = unique_temp_dir("sley-rev-parse");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<RevParseParity> {
        init_repo_with_format(&root, format)?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        sley_worktree::add_paths_to_index(
            &root,
            root.join(".git"),
            format,
            &[PathBuf::from("hello.txt")],
        )?;
        let identity = sley_sequencer::format_commit_identity(
            "Example User",
            "example@example.invalid",
            "@0 +0000",
        )?;
        let commit = sley_sequencer::commit_index(
            root.join(".git"),
            format,
            sley_sequencer::CommitIndexOptions {
                author: identity.clone(),
                committer: identity,
                message: b"initial subject\n".to_vec(),
                reflog_message: b"commit: initial subject".to_vec(),
                encoding: None,
            signature: None,
            },
        )?;
        let store = FileRefStore::new(root.join(".git"), format);
        store.create_branch(
            "feature",
            commit.oid,
            b"Git Rs <sley@example.invalid> 0 +0000".to_vec(),
            b"branch: Created from main".to_vec(),
        )?;
        store.create_tag("v1.0", commit.oid)?;
        let commit_hex = commit.oid.to_hex();
        let revs = ["HEAD", "main", "feature", "v1.0", commit_hex.as_str()];
        let upstream = String::from_utf8_lossy(&run_git_owned(
            &root,
            &[
                "rev-parse".into(),
                "HEAD".into(),
                "main".into(),
                "feature".into(),
                "v1.0".into(),
                commit_hex.clone(),
            ],
            &[],
        )?)
        .to_string();
        let mut rust = String::new();
        for rev in revs {
            rust.push_str(&format!(
                "{}\n",
                sley_rev::resolve_revision(root.join(".git"), format, rev)?
            ));
        }
        let short_upstream =
            String::from_utf8_lossy(&run_git(&root, ["rev-parse", "--short", "HEAD"], &[])?)
                .to_string();
        let short_rust = format!("{}\n", &commit_hex[..7]);
        let short_8_upstream =
            String::from_utf8_lossy(&run_git(&root, ["rev-parse", "--short=8", "v1.0"], &[])?)
                .to_string();
        let short_8_rust = format!("{}\n", &commit_hex[..8]);
        let short_min_upstream =
            String::from_utf8_lossy(&run_git(&root, ["rev-parse", "--short=0", "HEAD"], &[])?)
                .to_string();
        let short_min_rust = format!("{}\n", &commit_hex[..4]);
        let verify_upstream =
            String::from_utf8_lossy(&run_git(&root, ["rev-parse", "--verify", "HEAD"], &[])?)
                .to_string();
        let verify_rust = format!("{commit_hex}\n");
        let verify_quiet_upstream = String::from_utf8_lossy(&run_git(
            &root,
            ["rev-parse", "--verify", "--quiet", "HEAD"],
            &[],
        )?)
        .to_string();
        let verify_quiet_rust = format!("{commit_hex}\n");
        let verify_short_upstream = String::from_utf8_lossy(&run_git(
            &root,
            ["rev-parse", "--verify", "--short=8", "HEAD"],
            &[],
        )?)
        .to_string();
        let verify_short_rust = format!("{}\n", &commit_hex[..8]);
        let abbrev_ref_upstream = String::from_utf8_lossy(&run_git(
            &root,
            [
                "rev-parse",
                "--abbrev-ref",
                "HEAD",
                "refs/heads/main",
                "feature",
                "refs/tags/v1.0",
                "v1.0",
            ],
            &[],
        )?)
        .to_string();
        let abbrev_ref_rust = "main\nmain\nfeature\nv1.0\nv1.0\n".to_string();
        let symbolic_full_name_upstream = String::from_utf8_lossy(&run_git(
            &root,
            [
                "rev-parse",
                "--symbolic-full-name",
                "HEAD",
                "refs/heads/main",
                "feature",
                "refs/tags/v1.0",
                "v1.0",
                commit_hex.as_str(),
            ],
            &[],
        )?)
        .to_string();
        let symbolic_full_name_rust =
            "refs/heads/main\nrefs/heads/main\nrefs/heads/feature\nrefs/tags/v1.0\nrefs/tags/v1.0\n"
                .to_string();
        let top_level_upstream =
            String::from_utf8_lossy(&run_git(&root, ["rev-parse", "--show-toplevel"], &[])?)
                .to_string();
        let top_level_rust = format!("{}\n", fs::canonicalize(&root)?.display());
        let prefix_root_upstream =
            String::from_utf8_lossy(&run_git(&root, ["rev-parse", "--show-prefix"], &[])?)
                .to_string();
        let prefix_root_rust = "\n".to_string();
        let cdup_root_upstream =
            String::from_utf8_lossy(&run_git(&root, ["rev-parse", "--show-cdup"], &[])?)
                .to_string();
        let cdup_root_rust = "\n".to_string();
        fs::create_dir_all(root.join("src").join("nested"))?;
        let nested = root.join("src").join("nested");
        let prefix_nested_upstream =
            String::from_utf8_lossy(&run_git(&nested, ["rev-parse", "--show-prefix"], &[])?)
                .to_string();
        let prefix_nested_rust = "src/nested/\n".to_string();
        let cdup_nested_upstream =
            String::from_utf8_lossy(&run_git(&nested, ["rev-parse", "--show-cdup"], &[])?)
                .to_string();
        let cdup_nested_rust = "../../\n".to_string();
        let git_dir_upstream =
            String::from_utf8_lossy(&run_git(&nested, ["rev-parse", "--git-dir"], &[])?)
                .to_string();
        let absolute_git_dir_upstream =
            String::from_utf8_lossy(&run_git(&nested, ["rev-parse", "--absolute-git-dir"], &[])?)
                .to_string();
        let inside_work_tree_upstream = String::from_utf8_lossy(&run_git(
            &nested,
            ["rev-parse", "--is-inside-work-tree"],
            &[],
        )?)
        .to_string();
        let inside_git_dir_worktree_upstream = String::from_utf8_lossy(&run_git(
            &nested,
            ["rev-parse", "--is-inside-git-dir"],
            &[],
        )?)
        .to_string();
        let inside_git_dir_git_upstream = String::from_utf8_lossy(&run_git(
            &root.join(".git"),
            ["rev-parse", "--is-inside-git-dir"],
            &[],
        )?)
        .to_string();
        let bare_worktree_upstream = String::from_utf8_lossy(&run_git(
            &nested,
            ["rev-parse", "--is-bare-repository"],
            &[],
        )?)
        .to_string();
        let shallow_worktree_upstream = String::from_utf8_lossy(&run_git(
            &nested,
            ["rev-parse", "--is-shallow-repository"],
            &[],
        )?)
        .to_string();
        let shallow_worktree_rust = "false\n".to_string();
        fs::write(root.join(".git").join("shallow"), b"")?;
        let shallow_marker_upstream = String::from_utf8_lossy(&run_git(
            &nested,
            ["rev-parse", "--is-shallow-repository"],
            &[],
        )?)
        .to_string();
        let shallow_marker_rust = "true\n".to_string();
        let bare_root = root.join("bare.git");
        run_git_owned(
            &root,
            &[
                "init".into(),
                "-q".into(),
                "--bare".into(),
                bare_root.display().to_string(),
            ],
            &[],
        )?;
        fs::write(bare_root.join("shallow"), b"")?;
        let inside_git_dir_bare_upstream = String::from_utf8_lossy(&run_git(
            &bare_root,
            ["rev-parse", "--is-inside-git-dir"],
            &[],
        )?)
        .to_string();
        let bare_repo_upstream = String::from_utf8_lossy(&run_git(
            &bare_root,
            ["rev-parse", "--is-bare-repository"],
            &[],
        )?)
        .to_string();
        let shallow_bare_upstream = String::from_utf8_lossy(&run_git(
            &bare_root,
            ["rev-parse", "--is-shallow-repository"],
            &[],
        )?)
        .to_string();
        let git_dir_rust = format!("{}\n", fs::canonicalize(root.join(".git"))?.display());
        let absolute_git_dir_rust = git_dir_rust.clone();
        let inside_work_tree_rust = "true\n".to_string();
        let inside_git_dir_worktree_rust = "false\n".to_string();
        let inside_git_dir_git_rust = "true\n".to_string();
        let inside_git_dir_bare_rust = "true\n".to_string();
        let bare_worktree_rust = "false\n".to_string();
        let bare_repo_rust = "true\n".to_string();
        let shallow_bare_rust = "true\n".to_string();
        if rust != upstream
            || short_rust != short_upstream
            || short_8_rust != short_8_upstream
            || short_min_rust != short_min_upstream
            || verify_rust != verify_upstream
            || verify_quiet_rust != verify_quiet_upstream
            || verify_short_rust != verify_short_upstream
            || abbrev_ref_rust != abbrev_ref_upstream
            || symbolic_full_name_rust != symbolic_full_name_upstream
            || top_level_rust != top_level_upstream
            || prefix_root_rust != prefix_root_upstream
            || prefix_nested_rust != prefix_nested_upstream
            || cdup_root_rust != cdup_root_upstream
            || cdup_nested_rust != cdup_nested_upstream
            || git_dir_rust != git_dir_upstream
            || absolute_git_dir_rust != absolute_git_dir_upstream
            || inside_work_tree_rust != inside_work_tree_upstream
            || inside_git_dir_worktree_rust != inside_git_dir_worktree_upstream
            || inside_git_dir_git_rust != inside_git_dir_git_upstream
            || inside_git_dir_bare_rust != inside_git_dir_bare_upstream
            || bare_worktree_rust != bare_worktree_upstream
            || bare_repo_rust != bare_repo_upstream
            || shallow_worktree_rust != shallow_worktree_upstream
            || shallow_marker_rust != shallow_marker_upstream
            || shallow_bare_rust != shallow_bare_upstream
        {
            return Err(GitError::Command(format!(
                "rev-parse mismatch: expected {upstream:?}, got {rust:?}; short expected {short_upstream:?}, got {short_rust:?}; short=8 expected {short_8_upstream:?}, got {short_8_rust:?}; short=0 expected {short_min_upstream:?}, got {short_min_rust:?}; verify expected {verify_upstream:?}, got {verify_rust:?}; verify quiet expected {verify_quiet_upstream:?}, got {verify_quiet_rust:?}; verify short expected {verify_short_upstream:?}, got {verify_short_rust:?}; abbrev-ref expected {abbrev_ref_upstream:?}, got {abbrev_ref_rust:?}; symbolic-full-name expected {symbolic_full_name_upstream:?}, got {symbolic_full_name_rust:?}; show-toplevel expected {top_level_upstream:?}, got {top_level_rust:?}; show-prefix root expected {prefix_root_upstream:?}, got {prefix_root_rust:?}; show-prefix nested expected {prefix_nested_upstream:?}, got {prefix_nested_rust:?}; show-cdup root expected {cdup_root_upstream:?}, got {cdup_root_rust:?}; show-cdup nested expected {cdup_nested_upstream:?}, got {cdup_nested_rust:?}; git-dir expected {git_dir_upstream:?}, got {git_dir_rust:?}; absolute-git-dir expected {absolute_git_dir_upstream:?}, got {absolute_git_dir_rust:?}; is-inside-work-tree expected {inside_work_tree_upstream:?}, got {inside_work_tree_rust:?}; worktree is-inside-git-dir expected {inside_git_dir_worktree_upstream:?}, got {inside_git_dir_worktree_rust:?}; git dir is-inside-git-dir expected {inside_git_dir_git_upstream:?}, got {inside_git_dir_git_rust:?}; bare is-inside-git-dir expected {inside_git_dir_bare_upstream:?}, got {inside_git_dir_bare_rust:?}; worktree is-bare expected {bare_worktree_upstream:?}, got {bare_worktree_rust:?}; bare repo is-bare expected {bare_repo_upstream:?}, got {bare_repo_rust:?}; shallow worktree expected {shallow_worktree_upstream:?}, got {shallow_worktree_rust:?}; shallow marker expected {shallow_marker_upstream:?}, got {shallow_marker_rust:?}; shallow bare expected {shallow_bare_upstream:?}, got {shallow_bare_rust:?}"
            )));
        }
        Ok(RevParseParity {
            format,
            upstream,
            rust,
            short_upstream,
            short_rust,
            short_8_upstream,
            short_8_rust,
            short_min_upstream,
            short_min_rust,
            verify_upstream,
            verify_rust,
            verify_quiet_upstream,
            verify_quiet_rust,
            verify_short_upstream,
            verify_short_rust,
            abbrev_ref_upstream,
            abbrev_ref_rust,
            symbolic_full_name_upstream,
            symbolic_full_name_rust,
            top_level_upstream,
            top_level_rust,
            prefix_root_upstream,
            prefix_root_rust,
            prefix_nested_upstream,
            prefix_nested_rust,
            cdup_root_upstream,
            cdup_root_rust,
            cdup_nested_upstream,
            cdup_nested_rust,
            git_dir_upstream,
            git_dir_rust,
            absolute_git_dir_upstream,
            absolute_git_dir_rust,
            inside_work_tree_upstream,
            inside_work_tree_rust,
            inside_git_dir_worktree_upstream,
            inside_git_dir_worktree_rust,
            inside_git_dir_git_upstream,
            inside_git_dir_git_rust,
            inside_git_dir_bare_upstream,
            inside_git_dir_bare_rust,
            bare_worktree_upstream,
            bare_worktree_rust,
            bare_repo_upstream,
            bare_repo_rust,
            shallow_worktree_upstream,
            shallow_worktree_rust,
            shallow_marker_upstream,
            shallow_marker_rust,
            shallow_bare_upstream,
            shallow_bare_rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn rev_parse_object_format_parity() -> Result<RevParseObjectFormatParity> {
    let root = unique_temp_dir("sley-rev-parse-object-format");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<RevParseObjectFormatParity> {
        let sha1_root = root.join("sha1");
        let sha256_root = root.join("sha256");
        fs::create_dir_all(&sha1_root)?;
        fs::create_dir_all(&sha256_root)?;
        run_git(&sha1_root, ["init", "-q", "-b", "main"], &[])?;
        run_git(&sha256_root, ["init", "-q", "--object-format=sha256"], &[])?;

        let args = [
            "rev-parse",
            "--show-object-format",
            "--show-object-format=storage",
            "--show-object-format=input",
            "--show-object-format=output",
        ];
        let sha1_upstream = String::from_utf8_lossy(&run_git(&sha1_root, args, &[])?).to_string();
        let sha256_upstream =
            String::from_utf8_lossy(&run_git(&sha256_root, args, &[])?).to_string();
        let sha1_format =
            GitConfig::read(sha1_root.join(".git").join("config"))?.repository_object_format()?;
        let sha256_format =
            GitConfig::read(sha256_root.join(".git").join("config"))?.repository_object_format()?;
        let sha1_rust = rev_parse_object_format_output(sha1_format);
        let sha256_rust = rev_parse_object_format_output(sha256_format);

        if sha1_rust != sha1_upstream || sha256_rust != sha256_upstream {
            return Err(GitError::Command(format!(
                "rev-parse object-format mismatch: sha1 expected {sha1_upstream:?}, got {sha1_rust:?}; sha256 expected {sha256_upstream:?}, got {sha256_rust:?}"
            )));
        }

        Ok(RevParseObjectFormatParity {
            sha1_upstream,
            sha1_rust,
            sha256_upstream,
            sha256_rust,
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn rev_parse_object_format_output(format: ObjectFormat) -> String {
    let mut out = String::new();
    for _ in 0..4 {
        out.push_str(format.name());
        out.push('\n');
    }
    out
}

pub fn rev_parse_parent_parity() -> Result<RevParseParity> {
    rev_parse_parent_parity_for_format(ObjectFormat::Sha1)
}

pub fn rev_parse_parent_parity_sha256() -> Result<RevParseParity> {
    rev_parse_parent_parity_for_format(ObjectFormat::Sha256)
}

pub fn rev_parse_parent_parity_for_format(format: ObjectFormat) -> Result<RevParseParity> {
    let root = unique_temp_dir("sley-rev-parse-parents");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<RevParseParity> {
        match format {
            ObjectFormat::Sha1 => run_git(&root, ["init", "-q", "-b", "main"], &[])?,
            ObjectFormat::Sha256 => run_git(
                &root,
                ["init", "-q", "--object-format=sha256", "-b", "main"],
                &[],
            )?,
        };
        run_git(&root, ["config", "user.name", "Example User"], &[])?;
        run_git(
            &root,
            ["config", "user.email", "example@example.invalid"],
            &[],
        )?;
        fs::write(root.join("base.txt"), b"base\n")?;
        run_git(&root, ["add", "base.txt"], &[])?;
        run_git(
            &root,
            ["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"],
            &[],
        )?;
        run_git(&root, ["checkout", "-q", "-b", "side"], &[])?;
        fs::write(root.join("side.txt"), b"side\n")?;
        run_git(&root, ["add", "side.txt"], &[])?;
        run_git(
            &root,
            ["-c", "commit.gpgsign=false", "commit", "-q", "-m", "side"],
            &[],
        )?;
        run_git(&root, ["checkout", "-q", "main"], &[])?;
        fs::write(root.join("main.txt"), b"main\n")?;
        run_git(&root, ["add", "main.txt"], &[])?;
        run_git(
            &root,
            ["-c", "commit.gpgsign=false", "commit", "-q", "-m", "main"],
            &[],
        )?;
        run_git(
            &root,
            [
                "-c",
                "commit.gpgsign=false",
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "merge side",
                "side",
            ],
            &[],
        )?;
        let revs = [
            "HEAD", "HEAD^", "HEAD^1", "HEAD^2", "HEAD~", "HEAD~1", "HEAD~2", "HEAD^^", "HEAD^2~1",
        ];
        let upstream = String::from_utf8_lossy(&run_git(
            &root,
            [
                "rev-parse",
                "HEAD",
                "HEAD^",
                "HEAD^1",
                "HEAD^2",
                "HEAD~",
                "HEAD~1",
                "HEAD~2",
                "HEAD^^",
                "HEAD^2~1",
            ],
            &[],
        )?)
        .to_string();
        let mut rust = String::new();
        for rev in revs {
            rust.push_str(&format!(
                "{}\n",
                sley_rev::resolve_revision(root.join(".git"), format, rev)?
            ));
        }
        if rust != upstream {
            return Err(GitError::Command(format!(
                "rev-parse parent mismatch: expected {upstream:?}, got {rust:?}"
            )));
        }
        Ok(RevParseParity {
            format,
            upstream,
            rust,
            short_upstream: String::new(),
            short_rust: String::new(),
            short_8_upstream: String::new(),
            short_8_rust: String::new(),
            short_min_upstream: String::new(),
            short_min_rust: String::new(),
            verify_upstream: String::new(),
            verify_rust: String::new(),
            verify_quiet_upstream: String::new(),
            verify_quiet_rust: String::new(),
            verify_short_upstream: String::new(),
            verify_short_rust: String::new(),
            abbrev_ref_upstream: String::new(),
            abbrev_ref_rust: String::new(),
            symbolic_full_name_upstream: String::new(),
            symbolic_full_name_rust: String::new(),
            top_level_upstream: String::new(),
            top_level_rust: String::new(),
            prefix_root_upstream: String::new(),
            prefix_root_rust: String::new(),
            prefix_nested_upstream: String::new(),
            prefix_nested_rust: String::new(),
            cdup_root_upstream: String::new(),
            cdup_root_rust: String::new(),
            cdup_nested_upstream: String::new(),
            cdup_nested_rust: String::new(),
            git_dir_upstream: String::new(),
            git_dir_rust: String::new(),
            absolute_git_dir_upstream: String::new(),
            absolute_git_dir_rust: String::new(),
            inside_work_tree_upstream: String::new(),
            inside_work_tree_rust: String::new(),
            inside_git_dir_worktree_upstream: String::new(),
            inside_git_dir_worktree_rust: String::new(),
            inside_git_dir_git_upstream: String::new(),
            inside_git_dir_git_rust: String::new(),
            inside_git_dir_bare_upstream: String::new(),
            inside_git_dir_bare_rust: String::new(),
            bare_worktree_upstream: String::new(),
            bare_worktree_rust: String::new(),
            bare_repo_upstream: String::new(),
            bare_repo_rust: String::new(),
            shallow_worktree_upstream: String::new(),
            shallow_worktree_rust: String::new(),
            shallow_marker_upstream: String::new(),
            shallow_marker_rust: String::new(),
            shallow_bare_upstream: String::new(),
            shallow_bare_rust: String::new(),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

pub fn rev_parse_peel_parity() -> Result<RevParseParity> {
    rev_parse_peel_parity_for_format(ObjectFormat::Sha1)
}

pub fn rev_parse_peel_parity_sha256() -> Result<RevParseParity> {
    rev_parse_peel_parity_for_format(ObjectFormat::Sha256)
}

pub fn rev_parse_peel_parity_for_format(format: ObjectFormat) -> Result<RevParseParity> {
    let root = unique_temp_dir("sley-rev-parse-peel");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<RevParseParity> {
        init_repo_with_format(&root, format)?;
        run_git(&root, ["config", "user.name", "Example User"], &[])?;
        run_git(
            &root,
            ["config", "user.email", "example@example.invalid"],
            &[],
        )?;
        fs::write(root.join("hello.txt"), b"hello\n")?;
        run_git(&root, ["add", "hello.txt"], &[])?;
        run_git(
            &root,
            [
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "initial subject",
            ],
            &[],
        )?;
        run_git(
            &root,
            [
                "-c",
                "commit.gpgsign=false",
                "tag",
                "-a",
                "v1.0",
                "-m",
                "release",
            ],
            &[],
        )?;
        let revs = [
            "v1.0",
            "v1.0^{}",
            "v1.0^{object}",
            "v1.0^{tag}",
            "v1.0^{commit}",
            "v1.0^{tree}",
            "HEAD^{}",
            "HEAD^{commit}",
            "HEAD^{tree}",
        ];
        let upstream = String::from_utf8_lossy(&run_git(
            &root,
            [
                "rev-parse",
                "v1.0",
                "v1.0^{}",
                "v1.0^{object}",
                "v1.0^{tag}",
                "v1.0^{commit}",
                "v1.0^{tree}",
                "HEAD^{}",
                "HEAD^{commit}",
                "HEAD^{tree}",
            ],
            &[],
        )?)
        .to_string();
        let mut rust = String::new();
        for rev in revs {
            rust.push_str(&format!(
                "{}\n",
                sley_rev::resolve_revision(root.join(".git"), format, rev)?
            ));
        }
        if rust != upstream {
            return Err(GitError::Command(format!(
                "rev-parse peel mismatch: expected {upstream:?}, got {rust:?}"
            )));
        }
        Ok(RevParseParity {
            format,
            upstream,
            rust,
            short_upstream: String::new(),
            short_rust: String::new(),
            short_8_upstream: String::new(),
            short_8_rust: String::new(),
            short_min_upstream: String::new(),
            short_min_rust: String::new(),
            verify_upstream: String::new(),
            verify_rust: String::new(),
            verify_quiet_upstream: String::new(),
            verify_quiet_rust: String::new(),
            verify_short_upstream: String::new(),
            verify_short_rust: String::new(),
            abbrev_ref_upstream: String::new(),
            abbrev_ref_rust: String::new(),
            symbolic_full_name_upstream: String::new(),
            symbolic_full_name_rust: String::new(),
            top_level_upstream: String::new(),
            top_level_rust: String::new(),
            prefix_root_upstream: String::new(),
            prefix_root_rust: String::new(),
            prefix_nested_upstream: String::new(),
            prefix_nested_rust: String::new(),
            cdup_root_upstream: String::new(),
            cdup_root_rust: String::new(),
            cdup_nested_upstream: String::new(),
            cdup_nested_rust: String::new(),
            git_dir_upstream: String::new(),
            git_dir_rust: String::new(),
            absolute_git_dir_upstream: String::new(),
            absolute_git_dir_rust: String::new(),
            inside_work_tree_upstream: String::new(),
            inside_work_tree_rust: String::new(),
            inside_git_dir_worktree_upstream: String::new(),
            inside_git_dir_worktree_rust: String::new(),
            inside_git_dir_git_upstream: String::new(),
            inside_git_dir_git_rust: String::new(),
            inside_git_dir_bare_upstream: String::new(),
            inside_git_dir_bare_rust: String::new(),
            bare_worktree_upstream: String::new(),
            bare_worktree_rust: String::new(),
            bare_repo_upstream: String::new(),
            bare_repo_rust: String::new(),
            shallow_worktree_upstream: String::new(),
            shallow_worktree_rust: String::new(),
            shallow_marker_upstream: String::new(),
            shallow_marker_rust: String::new(),
            shallow_bare_upstream: String::new(),
            shallow_bare_rust: String::new(),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn upstream_git_hash_object(
    format: ObjectFormat,
    object_type: &str,
    body: &[u8],
) -> Result<String> {
    if format == ObjectFormat::Sha1 {
        return upstream_git_hash_object_in_dir(Path::new("."), object_type, body);
    }
    let root = unique_temp_dir("sley-hash-object-sha256");
    fs::create_dir_all(&root)?;
    let result = (|| -> Result<String> {
        run_git(&root, ["init", "-q", "--object-format=sha256"], &[])?;
        upstream_git_hash_object_in_dir(&root, object_type, body)
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

fn upstream_git_hash_object_in_dir(cwd: &Path, object_type: &str, body: &[u8]) -> Result<String> {
    let mut child = hermetic_git_command(oracle_git())
        .args(["hash-object", "-t", object_type, "--stdin"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(err.to_string()))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| GitError::Command("missing git stdin".into()))?
        .write_all(body)?;
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn init_repo_for_format(cwd: &Path, format: ObjectFormat) -> Result<()> {
    match format {
        ObjectFormat::Sha1 => run_git(cwd, ["init", "-q", "-b", "main"], &[]),
        ObjectFormat::Sha256 => run_git(cwd, ["init", "-q", "--object-format=sha256"], &[]),
    }
    .map(|_| ())
}

/// Write `bytes` to a spawned child's stdin, tolerating the child closing the
/// pipe before draining its input.
///
/// Integration tests pipe input to `git` (and `sley`) subprocesses and then call
/// `wait_with_output` to capture the real result. But both `git` and `sley`
/// legitimately exit *before* reading stdin when the arguments are a usage or
/// option error — they print to stderr and exit non-zero without draining fd 0.
/// When that happens the kernel tears down the pipe and our `write_all` races
/// the child's close, surfacing `io::ErrorKind::BrokenPipe`. That is not a test
/// failure: the child's actual exit status and output are still captured by the
/// subsequent `wait_with_output`, so swallowing the broken pipe here is correct
/// robustness, not masking. (Verified by strace on the interpret-trailers usage
/// path: git exits on the bad option before reading fd 0.)
///
/// Any *other* write error is a genuine harness fault and panics with context.
pub fn write_stdin_tolerating_early_exit(stdin: &mut std::process::ChildStdin, bytes: &[u8]) {
    match stdin.write_all(bytes) {
        Ok(()) => {}
        // The child exited before draining stdin (usage/option error path). Its
        // real result is still captured by `wait_with_output`; ignore the race.
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(err) => panic!("failed to write child stdin: {err}"),
    }
}

fn run_git<const N: usize>(cwd: &Path, args: [&str; N], stdin: &[u8]) -> Result<Vec<u8>> {
    let mut child = hermetic_git_command(oracle_git())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(err.to_string()))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| GitError::Command("missing git stdin".into()))?
        .write_all(stdin)?;
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(output.stdout)
}

fn run_git_with_env<const N: usize, const M: usize>(
    cwd: &Path,
    args: [&str; N],
    stdin: &[u8],
    env: [(&str, &str); M],
) -> Result<Vec<u8>> {
    let mut command = hermetic_git_command(oracle_git());
    command.args(args).current_dir(cwd);
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(err.to_string()))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| GitError::Command("missing git stdin".into()))?
        .write_all(stdin)?;
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(output.stdout)
}

fn run_git_owned(cwd: &Path, args: &[String], stdin: &[u8]) -> Result<Vec<u8>> {
    let mut child = hermetic_git_command(oracle_git())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(err.to_string()))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| GitError::Command("missing git stdin".into()))?
        .write_all(stdin)?;
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(output.stdout)
}

fn init_repo_with_format(root: &Path, format: ObjectFormat) -> Result<Vec<u8>> {
    match format {
        ObjectFormat::Sha1 => run_git(root, ["init", "-q", "-b", "main"], &[]),
        ObjectFormat::Sha256 => run_git(
            root,
            ["init", "-q", "-b", "main", "--object-format=sha256"],
            &[],
        ),
    }
}

fn zero_oid(format: ObjectFormat) -> Result<ObjectId> {
    Ok(ObjectId::null(format))
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{}-{}-{}",
        prefix,
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Harness for running UPSTREAM git's own `t/*.sh` test suite against the
/// sley binary.
///
/// Upstream git ships a TAP-emitting shell test framework (`t/test-lib.sh` plus
/// `t/tNNNN-*.sh` scripts). `test-lib.sh` can exercise an externally installed
/// git via the `GIT_TEST_INSTALLED` environment variable, which must point at a
/// directory containing a working `git` executable. This module drives
/// `scripts/run-upstream-tests.sh`, which builds such a directory whose `git`
/// is a shim around the sley binary, runs a configurable subset of upstream
/// scripts against it, and aggregates the results. Running the upstream suite is
/// the ultimate parity oracle.
///
/// # Environment / layout
///
/// Point the harness at an upstream git source checkout's `t/` directory via one
/// of these environment variables:
///
/// * `SLEY_UPSTREAM_T` — absolute path to the upstream git `t/` directory.
/// * `GIT_RS_UPSTREAM_T` — legacy alias for `SLEY_UPSTREAM_T`.
/// * `GIT_SRC_DIR` — absolute path to a git source root (we use `$GIT_SRC_DIR/t`).
///
/// The `t/` directory must come from a *built* checkout: `test-lib.sh` sources
/// `GIT-BUILD-OPTIONS` and requires `t/helper/test-tool` and the `templates/blt`
/// directory, all of which a build produces. See `run_upstream_default_subset`
/// for a worked example of how to prepare one.
pub mod upstream {
    use sley_core::{GitError, Result};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// The default foundational subset of upstream scripts.
    ///
    /// These target commands sley already implements (init, cat-file,
    /// hash-object, config, rev-parse, ls-files, ls-tree, symbolic-ref) and are
    /// small enough that a run finishes quickly. Names are exact upstream
    /// filenames as of git master.
    pub const DEFAULT_SCRIPTS: &[&str] = &[
        "t0001-init.sh",
        "t1006-cat-file.sh",
        "t1007-hash-object.sh",
        "t1300-config.sh",
        "t1500-rev-parse.sh",
        "t3000-ls-files-others.sh",
        "t3103-ls-tree-misc.sh",
        "t1401-symbolic-ref.sh",
    ];

    /// Maps each foundational git subcommand to the single upstream script that
    /// exercises it. This is the inverse of the runner's `command_alias` table;
    /// keep the two in sync. Callers can target one command by name via
    /// [`script_for_command`] / [`run_upstream_command`] instead of memorising
    /// `tNNNN` numbers.
    pub const FOUNDATIONAL_COMMANDS: &[(&str, &str)] = &[
        ("init", "t0001-init.sh"),
        ("cat-file", "t1006-cat-file.sh"),
        ("hash-object", "t1007-hash-object.sh"),
        ("config", "t1300-config.sh"),
        ("rev-parse", "t1500-rev-parse.sh"),
        ("ls-files", "t3000-ls-files-others.sh"),
        ("ls-tree", "t3103-ls-tree-misc.sh"),
        ("symbolic-ref", "t1401-symbolic-ref.sh"),
    ];

    /// Resolve a foundational command name (e.g. `"config"`) to its upstream
    /// script basename (e.g. `"t1300-config.sh"`). Returns `None` for unknown
    /// names. The runner also accepts the command name directly as an argument.
    pub fn script_for_command(command: &str) -> Option<&'static str> {
        FOUNDATIONAL_COMMANDS
            .iter()
            .find(|(name, _)| *name == command)
            .map(|(_, script)| *script)
    }

    /// Resolve an upstream script basename back to its friendly command name.
    pub fn command_for_script(script: &str) -> Option<&'static str> {
        FOUNDATIONAL_COMMANDS
            .iter()
            .find(|(_, s)| *s == script)
            .map(|(name, _)| *name)
    }

    /// Per-script result parsed from the runner's output.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ScriptResult {
        /// Upstream script basename, e.g. `t0001-init.sh`.
        pub script: String,
        /// Friendly command name (e.g. `config`) when the script is one of the
        /// foundational subset; otherwise falls back to the script basename.
        pub command: String,
        /// `PASS`, `FAIL`, or `TIMEOUT`.
        pub result: String,
        /// Count of TAP `ok` assertions.
        pub ok: u32,
        /// Count of TAP `not ok` assertions.
        pub failed: u32,
    }

    impl ScriptResult {
        /// Total assertions actually run (`ok + not ok`). Note this can be less
        /// than the script's TAP plan when the script aborted or timed out
        /// partway through.
        pub fn total(&self) -> u32 {
            self.ok + self.failed
        }

        /// Assertion pass rate in percent (0–100), or 0 when nothing ran.
        pub fn pass_rate(&self) -> u32 {
            // `checked_div` yields `None` (-> 0) when no assertions ran, avoiding
            // a divide-by-zero without a manual guard.
            (self.ok.saturating_mul(100))
                .checked_div(self.total())
                .unwrap_or(0)
        }
    }

    /// Outcome of attempting to run the upstream suite.
    #[derive(Debug, Clone)]
    pub enum UpstreamRunOutcome {
        /// No upstream `t/` directory was configured (neither `SLEY_UPSTREAM_T`,
        /// its legacy `GIT_RS_UPSTREAM_T` alias, nor `GIT_SRC_DIR` is set). Holds
        /// a human-readable reason. This is a clean skip, not a failure.
        Skipped(String),
        /// The runner executed. Holds the parsed per-script results and the path
        /// to the full text report, plus whether every script passed.
        Ran {
            results: Vec<ScriptResult>,
            report_path: PathBuf,
            all_passed: bool,
        },
    }

    /// Resolve the upstream git `t/` directory from the environment, returning
    /// `None` (rather than an error) when nothing is configured so callers can
    /// skip cleanly.
    pub fn upstream_t_dir() -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("SLEY_UPSTREAM_T")
            && !dir.is_empty()
        {
            return Some(PathBuf::from(dir));
        }
        if let Ok(dir) = std::env::var("GIT_RS_UPSTREAM_T")
            && !dir.is_empty()
        {
            return Some(PathBuf::from(dir));
        }
        if let Ok(root) = std::env::var("GIT_SRC_DIR")
            && !root.is_empty()
        {
            return Some(PathBuf::from(root).join("t"));
        }
        None
    }

    /// Absolute path to `scripts/run-upstream-tests.sh` shipped with this crate.
    pub fn runner_script_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join("run-upstream-tests.sh")
    }

    /// Run the upstream suite over the default foundational subset.
    ///
    /// Returns [`UpstreamRunOutcome::Skipped`] when no upstream `t/` directory is
    /// configured; otherwise runs the suite and parses the per-script results.
    /// `sley_bin`, when provided, is exported as `SLEY_BIN` so the runner
    /// uses exactly that binary (handy from an integration test via
    /// `env!("CARGO_BIN_EXE_sley")`); otherwise the runner resolves the binary
    /// itself.
    pub fn run_upstream_default_subset(sley_bin: Option<&Path>) -> Result<UpstreamRunOutcome> {
        run_upstream_scripts(DEFAULT_SCRIPTS, sley_bin)
    }

    /// Like [`run_upstream_default_subset`] but with an explicit script list.
    /// Each entry may be a foundational command name (`config`), a basename
    /// (`t0001-init.sh`), a numeric prefix (`t0001`), or a glob (`t13*`); the
    /// runner resolves it against the upstream `t/` directory.
    pub fn run_upstream_scripts(
        scripts: &[&str],
        sley_bin: Option<&Path>,
    ) -> Result<UpstreamRunOutcome> {
        run_upstream_scripts_labeled(scripts, sley_bin, None)
    }

    /// Run a single foundational command's upstream script by name (e.g.
    /// `"config"`, `"cat-file"`). The runner also accepts the command name
    /// directly, so this is a thin, discoverable wrapper. `label`, when given,
    /// is recorded in the report and the append-only pass-rate history so trends
    /// are attributable across runs (e.g. a git short-SHA).
    pub fn run_upstream_command(
        command: &str,
        sley_bin: Option<&Path>,
        label: Option<&str>,
    ) -> Result<UpstreamRunOutcome> {
        run_upstream_scripts_labeled(&[command], sley_bin, label)
    }

    /// Like [`run_upstream_scripts`] but also records a caller-provided `label`
    /// (passed through as `SLEY_RUN_LABEL`) in the report and history. The
    /// library deliberately never reads a clock; when `label` is `None` the
    /// runner script supplies a UTC timestamp at the shell layer.
    pub fn run_upstream_scripts_labeled(
        scripts: &[&str],
        sley_bin: Option<&Path>,
        label: Option<&str>,
    ) -> Result<UpstreamRunOutcome> {
        if upstream_t_dir().is_none() {
            return Ok(UpstreamRunOutcome::Skipped(
                "no upstream git t/ directory configured; \
                 set SLEY_UPSTREAM_T (path to git's t/ dir) or \
                 GIT_SRC_DIR (a built git source root, we use $GIT_SRC_DIR/t)"
                    .into(),
            ));
        }

        let runner = runner_script_path();
        if !runner.exists() {
            return Err(GitError::not_found(format!(
                "runner script missing: {}",
                runner.display()
            )));
        }

        let mut command = Command::new("sh");
        command.arg(&runner);
        for script in scripts {
            command.arg(script);
        }
        if let Some(bin) = sley_bin {
            command.env("SLEY_BIN", bin);
        }
        if let Some(label) = label {
            command.env("SLEY_RUN_LABEL", label);
        }

        let output = command
            .output()
            .map_err(|err| GitError::Command(format!("failed to spawn runner: {err}")))?;

        // The runner prints the per-script result table to stdout and logs the
        // report path to stderr; parse each from its respective stream.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let results = parse_results(&stdout);
        let report_path = report_path_from_logs(&stdout)
            .or_else(|| report_path_from_logs(&stderr))
            .unwrap_or_else(default_report_path);

        Ok(UpstreamRunOutcome::Ran {
            results,
            report_path,
            // The runner exits 0 only when every selected script passed.
            all_passed: output.status.success(),
        })
    }

    fn default_report_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("upstream-report.txt")
    }

    fn report_path_from_logs(text: &str) -> Option<PathBuf> {
        text.lines().find_map(|line| {
            line.strip_prefix("Full report written to: ")
                .map(|path| PathBuf::from(path.trim()))
        })
    }

    /// Parse the per-script result rows the runner prints, e.g.:
    /// `t0001-init.sh                FAIL        39    63  rc=1 ...`
    pub(crate) fn parse_results(stdout: &str) -> Vec<ScriptResult> {
        let mut results = Vec::new();
        for line in stdout.lines() {
            let mut fields = line.split_whitespace();
            let Some(script) = fields.next() else {
                continue;
            };
            // Rows start with an upstream script basename.
            if !(script.starts_with('t') && script.ends_with(".sh")) {
                continue;
            }
            let Some(result) = fields.next() else {
                continue;
            };
            if !matches!(result, "PASS" | "FAIL" | "TIMEOUT") {
                continue;
            }
            let ok = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let failed = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let command = command_for_script(script)
                .map(str::to_string)
                .unwrap_or_else(|| script.to_string());
            results.push(ScriptResult {
                script: script.to_string(),
                command,
                result: result.to_string(),
                ok,
                failed,
            });
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermetic_git_command_ignores_ambient_config_sources() {
        let root = unique_temp_dir("sley-hermetic-git-config");
        fs::create_dir_all(&root).expect("create temp root");
        let global = root.join("global.gitconfig");
        let system = root.join("system.gitconfig");
        fs::write(&global, "[user]\n\tname = Host User\n").expect("write global config");
        fs::write(&system, "[core]\n\teditor = host-editor\n").expect("write system config");

        let mut global_command = std::process::Command::new(oracle_git());
        global_command
            .current_dir(&root)
            .args(["config", "--get", "user.name"])
            .env("GIT_CONFIG_GLOBAL", &global);
        apply_hermetic_git_env(&mut global_command);
        let global_output = global_command.output().expect("run git config");
        assert!(!global_output.status.success());
        assert!(global_output.stdout.is_empty());

        let mut system_command = std::process::Command::new(oracle_git());
        system_command
            .current_dir(&root)
            .args(["config", "--get", "core.editor"])
            .env("GIT_CONFIG_SYSTEM", &system);
        apply_hermetic_git_env(&mut system_command);
        let system_output = system_command.output().expect("run git config");
        assert!(!system_output.status.success());
        assert!(system_output.stdout.is_empty());

        let mut injected_command = std::process::Command::new(oracle_git());
        injected_command
            .current_dir(&root)
            .args(["config", "--get", "pair.one"])
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "pair.one")
            .env("GIT_CONFIG_VALUE_0", "from-env");
        apply_hermetic_git_env(&mut injected_command);
        let injected_output = injected_command.output().expect("run git config");
        assert!(!injected_output.status.success());
        assert!(injected_output.stdout.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hermetic_git_command_allows_explicit_config_overlay_after_construction() {
        let root = unique_temp_dir("sley-hermetic-git-overlay");
        fs::create_dir_all(&root).expect("create temp root");
        let output = hermetic_git_command(oracle_git())
            .current_dir(&root)
            .args(["config", "--get", "pair.one"])
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "pair.one")
            .env("GIT_CONFIG_VALUE_0", "from-env")
            .output()
            .expect("run git config");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"from-env\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hermetic_git_command_with_identity_sets_standard_identity() {
        let root = unique_temp_dir("sley-hermetic-git-identity");
        fs::create_dir_all(&root).expect("create temp root");

        let author = hermetic_git_command_with_identity(oracle_git())
            .current_dir(&root)
            .args(["var", "GIT_AUTHOR_IDENT"])
            .output()
            .expect("run git var");
        assert!(author.status.success());
        assert_eq!(
            author.stdout,
            format!("{TEST_GIT_USER_NAME} <{TEST_GIT_USER_EMAIL}> 0 +0000\n").into_bytes()
        );

        let committer = hermetic_git_command_with_identity(oracle_git())
            .current_dir(&root)
            .args(["var", "GIT_COMMITTER_IDENT"])
            .output()
            .expect("run git var");
        assert!(committer.status.success());
        assert_eq!(
            committer.stdout,
            format!("{TEST_GIT_USER_NAME} <{TEST_GIT_USER_EMAIL}> 0 +0000\n").into_bytes()
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn default_hash_object_cases_match_upstream_git() {
        hash_object_parity(&default_hash_object_cases()).expect("test operation should succeed");
    }

    #[test]
    fn default_hash_object_cases_match_upstream_git_sha256() {
        hash_object_parity_for_format(ObjectFormat::Sha256, &default_hash_object_cases())
            .expect("test operation should succeed");
    }

    #[test]
    fn reads_upstream_single_blob_pack() {
        let result = single_blob_pack_read_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.object_type, "blob");
        assert_eq!(result.body, b"hello from pack\n");
    }

    #[test]
    fn reads_upstream_single_blob_pack_sha256() {
        let result = single_blob_pack_read_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.object_type, "blob");
        assert_eq!(result.body, b"hello from pack\n");
    }

    #[test]
    fn reads_upstream_single_blob_pack_index() {
        let result = single_blob_pack_index_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.entries, 1);
        assert_eq!(result.offset, 12);
    }

    #[test]
    fn reads_upstream_single_blob_pack_index_sha256() {
        let result = single_blob_pack_index_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.entries, 1);
        assert_eq!(result.offset, 12);
    }

    #[test]
    fn reads_upstream_delta_pack() {
        let result = delta_pack_read_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.entries, 2);
        assert!(result.delta_entries >= 1);
    }

    #[test]
    fn reads_upstream_delta_pack_sha256() {
        let result = delta_pack_read_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.entries, 2);
        assert!(result.delta_entries >= 1);
    }

    #[test]
    fn reads_upstream_thin_pack_with_external_base() {
        let result = thin_pack_read_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_thin_pack_read_parity(result);
    }

    #[test]
    fn reads_upstream_thin_pack_with_external_base_sha256() {
        let result = thin_pack_read_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_thin_pack_read_parity(result);
    }

    fn assert_thin_pack_read_parity(result: ThinPackReadParity) {
        assert_eq!(result.entries, 3);
        assert_eq!(result.base_oid.len(), result.format.hex_len());
        assert_eq!(result.changed_oid.len(), result.format.hex_len());
    }

    #[test]
    fn upstream_git_reads_loose_ref_written_by_rust_store() {
        let result = loose_ref_interop_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_ref_interop_parity(result);
    }

    #[test]
    fn upstream_git_reads_loose_sha256_ref_written_by_rust_store() {
        let result = loose_ref_interop_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_ref_interop_parity(result);
    }

    fn assert_ref_interop_parity(result: RefInteropParity) {
        assert_eq!(
            result.upstream_show_ref,
            format!("{} {}\n", result.oid, result.name)
        );
    }

    #[test]
    fn upstream_git_reads_packed_ref_written_by_rust_store() {
        let result = packed_ref_interop_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_ref_interop_parity(result);
    }

    #[test]
    fn upstream_git_reads_packed_sha256_ref_written_by_rust_store() {
        let result = packed_ref_interop_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_ref_interop_parity(result);
    }

    #[test]
    fn upstream_git_reads_compacted_packed_ref_written_by_rust_store() {
        let result = packed_ref_compaction_interop_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_ref_interop_parity(result);
    }

    #[test]
    fn upstream_git_reads_compacted_packed_sha256_ref_written_by_rust_store() {
        let result =
            packed_ref_compaction_interop_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_ref_interop_parity(result);
    }

    #[test]
    fn upstream_git_reads_peeled_compacted_packed_ref_written_by_rust_store() {
        let result =
            peeled_packed_ref_compaction_interop_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_peeled_packed_ref_interop_parity(result);
    }

    #[test]
    fn upstream_git_reads_peeled_compacted_packed_sha256_ref_written_by_rust_store() {
        let result = peeled_packed_ref_compaction_interop_parity_sha256()
            .expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_peeled_packed_ref_interop_parity(result);
    }

    fn assert_peeled_packed_ref_interop_parity(result: PeeledPackedRefInteropParity) {
        assert_eq!(
            result.upstream_show_ref,
            format!(
                "{} {}\n{} {}^{{}}\n",
                result.tag_oid, result.name, result.peeled_oid, result.name
            )
        );
    }

    #[test]
    fn rust_show_ref_filters_match_upstream_git() {
        let result = show_ref_filter_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_show_ref_filter_parity(result);
    }

    #[test]
    fn rust_show_ref_filters_match_upstream_git_sha256() {
        let result = show_ref_filter_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_show_ref_filter_parity(result);
    }

    fn assert_show_ref_filter_parity(result: ShowRefFilterParity) {
        assert_eq!(result.heads_rust, result.heads_upstream);
        assert_eq!(result.tags_rust, result.tags_upstream);
        assert_eq!(result.heads_hash_rust, result.heads_hash_upstream);
        assert_eq!(result.tags_hash_rust, result.tags_hash_upstream);
        assert_eq!(result.heads_abbrev_rust, result.heads_abbrev_upstream);
        assert_eq!(
            result.tags_hash_abbrev_rust,
            result.tags_hash_abbrev_upstream
        );
        assert_eq!(result.tags_deref_rust, result.tags_deref_upstream);
        assert_eq!(result.tags_deref_hash_rust, result.tags_deref_hash_upstream);
        assert_eq!(
            result.tags_deref_abbrev_rust,
            result.tags_deref_abbrev_upstream
        );
    }

    #[test]
    fn rust_show_ref_verify_matches_upstream_git() {
        let result = show_ref_verify_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_show_ref_verify_parity(result);
    }

    #[test]
    fn rust_show_ref_verify_matches_upstream_git_sha256() {
        let result = show_ref_verify_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_show_ref_verify_parity(result);
    }

    fn assert_show_ref_verify_parity(result: ShowRefVerifyParity) {
        assert_eq!(result.rust, result.upstream);
        assert_eq!(result.hash_rust, result.hash_upstream);
        assert_eq!(result.deref_rust, result.deref_upstream);
        assert_eq!(result.quiet_rust, result.quiet_upstream);
        assert!(result.quiet_rust.is_empty());
    }

    #[test]
    fn rust_symbolic_ref_matches_upstream_git() {
        let result = symbolic_ref_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_symbolic_ref_parity(result);
    }

    #[test]
    fn rust_symbolic_ref_matches_upstream_git_sha256() {
        let result = symbolic_ref_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_symbolic_ref_parity(result);
    }

    fn assert_symbolic_ref_parity(result: SymbolicRefParity) {
        assert_eq!(result.head_rust, result.head_upstream);
        assert_eq!(result.short_rust, result.short_upstream);
        assert_eq!(result.switched_rust, result.switched_upstream);
    }

    #[test]
    fn upstream_git_reads_rust_written_pack() {
        let result = rust_pack_write_interop_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.upstream_body, b"hello from rust pack writer\n");
    }

    #[test]
    fn upstream_git_reads_rust_written_pack_sha256() {
        let result =
            rust_pack_write_interop_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.upstream_body, b"hello from rust pack writer\n");
    }

    #[test]
    fn upstream_git_reads_rust_written_bundle() {
        let result = rust_bundle_write_interop_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert!(result.heads.contains(" refs/heads/main\n"));
        assert!(
            result
                .verify_stdout
                .contains("The bundle records a complete history.")
        );
        assert_eq!(result.upstream_body, b"hello from rust bundle writer\n");
    }

    #[test]
    fn upstream_git_reads_rust_written_bundle_sha256() {
        let result =
            rust_bundle_write_interop_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert!(result.heads.contains(" refs/heads/main\n"));
        assert!(
            result
                .verify_stdout
                .contains("The bundle uses this hash algorithm: sha256")
        );
        assert_eq!(result.upstream_body, b"hello from rust bundle writer\n");
    }

    #[test]
    fn upstream_git_reads_rust_written_delta_pack() {
        let result = rust_delta_pack_write_interop_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert!(result.delta_entries >= 1);
    }

    #[test]
    fn upstream_git_reads_rust_written_delta_pack_sha256() {
        let result =
            rust_delta_pack_write_interop_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert!(result.delta_entries >= 1);
    }

    #[test]
    fn upstream_git_reads_rust_written_sha256_loose_object() {
        let result = sha256_loose_object_interop_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.upstream_type, "blob");
        assert_eq!(result.upstream_body, b"hello from sha256 loose object\n");
    }

    #[test]
    fn rust_odb_reads_upstream_git_pack() {
        let result = packed_odb_read_interop_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.body, b"hello from upstream pack\n");
    }

    #[test]
    fn rust_odb_reads_upstream_git_pack_sha256() {
        let result =
            packed_odb_read_interop_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.body, b"hello from upstream pack\n");
    }

    #[test]
    fn rust_odb_reads_upstream_delta_pack() {
        let result = delta_packed_odb_read_interop_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.entries, 2);
        assert!(result.delta_entries >= 1);
    }

    #[test]
    fn rust_odb_reads_upstream_delta_pack_sha256() {
        let result =
            delta_packed_odb_read_interop_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.entries, 2);
        assert!(result.delta_entries >= 1);
    }

    #[test]
    fn parses_upstream_git_repository_config() {
        let result = repository_config_interop_parity().expect("test operation should succeed");
        assert_eq!(result.object_format, ObjectFormat::Sha256);
        assert_eq!(result.bare, Some(false));
    }

    #[test]
    fn rust_ls_tree_matches_upstream_git() {
        let result = ls_tree_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_ls_tree_parity(result);
    }

    #[test]
    fn rust_ls_tree_matches_upstream_git_sha256() {
        let result = ls_tree_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_ls_tree_parity(result);
    }

    fn assert_ls_tree_parity(result: LsTreeParity) {
        assert_eq!(result.rust, result.upstream);
        assert_eq!(result.name_only_rust, result.name_only_upstream);
        assert_eq!(result.object_only_rust, result.object_only_upstream);
        assert_eq!(result.long_rust, result.long_upstream);
        assert_eq!(result.recursive_rust, result.recursive_upstream);
        assert_eq!(
            result.recursive_object_only_rust,
            result.recursive_object_only_upstream
        );
        assert_eq!(result.recursive_long_rust, result.recursive_long_upstream);
        assert_eq!(
            result.recursive_name_only_rust,
            result.recursive_name_only_upstream
        );
        assert_eq!(result.z_rust, result.z_upstream);
        assert_eq!(result.name_only_z_rust, result.name_only_z_upstream);
        assert_eq!(result.object_only_z_rust, result.object_only_z_upstream);
        assert_eq!(result.long_z_rust, result.long_z_upstream);
        assert_eq!(result.recursive_z_rust, result.recursive_z_upstream);
        assert_eq!(
            result.recursive_object_only_z_rust,
            result.recursive_object_only_z_upstream
        );
        assert_eq!(
            result.recursive_long_z_rust,
            result.recursive_long_z_upstream
        );
        assert_eq!(
            result.recursive_name_only_z_rust,
            result.recursive_name_only_z_upstream
        );
    }

    #[test]
    fn rust_log_matches_minimal_upstream_git_format() {
        let result = log_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.rust, result.upstream);
    }

    #[test]
    fn rust_log_matches_minimal_upstream_git_format_sha256() {
        let result = log_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.rust, result.upstream);
    }

    #[test]
    fn rust_cat_file_resolves_revisions_like_upstream_git() {
        let result = cat_file_revision_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.rust, result.upstream);
        assert_eq!(&result.revs[..2], ["HEAD", "refs/tags/v2.0"]);
        assert_eq!(result.revs[2].len(), result.format.hex_len());
    }

    #[test]
    fn rust_cat_file_resolves_revisions_like_upstream_git_sha256() {
        let result = cat_file_revision_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.rust, result.upstream);
        assert_eq!(&result.revs[..2], ["HEAD", "refs/tags/v2.0"]);
        assert_eq!(result.revs[2].len(), result.format.hex_len());
    }

    #[test]
    fn upstream_index_round_trips_byte_for_byte() {
        let result = index_round_trip_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.entries, 1);
        assert!(result.byte_len > 20);
    }

    #[test]
    fn upstream_sha256_index_round_trips_byte_for_byte() {
        let result = index_round_trip_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.entries, 1);
        assert!(result.byte_len > 32);
    }

    #[test]
    fn upstream_git_reads_rust_update_index_add() {
        let result = update_index_add_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.upstream, result.expected);
    }

    #[test]
    fn upstream_git_reads_rust_sha256_update_index_add() {
        let result = update_index_add_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.upstream, result.expected);
    }

    #[test]
    fn rust_ls_files_stage_matches_upstream_git() {
        let result = ls_files_stage_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_ls_files_stage_parity(result);
    }

    #[test]
    fn rust_ls_files_stage_matches_upstream_git_sha256() {
        let result = ls_files_stage_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_ls_files_stage_parity(result);
    }

    fn assert_ls_files_stage_parity(result: LsFilesStageParity) {
        assert_eq!(result.rust, result.upstream);
        assert_eq!(result.rust_z, result.upstream_z);
        assert_eq!(result.rust_stage_z, result.upstream_stage_z);
        assert_eq!(result.rust_cached, result.upstream_cached);
        assert_eq!(result.rust_cached_z, result.upstream_cached_z);
        assert_eq!(result.rust_others, result.upstream_others);
        assert_eq!(result.rust_others_z, result.upstream_others_z);
        assert_eq!(result.rust_stage_others, result.upstream_stage_others);
        assert_eq!(result.rust_cached_others, result.upstream_cached_others);
        assert_eq!(result.rust_deleted, result.upstream_deleted);
        assert_eq!(result.rust_deleted_z, result.upstream_deleted_z);
        assert_eq!(result.rust_stage_deleted, result.upstream_stage_deleted);
        assert_eq!(result.rust_others_deleted, result.upstream_others_deleted);
        assert_eq!(
            result.rust_stage_others_deleted,
            result.upstream_stage_others_deleted
        );
        assert_eq!(result.rust_modified, result.upstream_modified);
        assert_eq!(result.rust_modified_z, result.upstream_modified_z);
        assert_eq!(result.rust_stage_modified, result.upstream_stage_modified);
        assert_eq!(result.rust_cached_modified, result.upstream_cached_modified);
        assert_eq!(
            result.rust_deleted_modified,
            result.upstream_deleted_modified
        );
        assert_eq!(
            result.rust_cached_deleted_modified,
            result.upstream_cached_deleted_modified
        );
        assert_eq!(
            result.rust_deduplicate_deleted_modified,
            result.upstream_deduplicate_deleted_modified
        );
        assert_eq!(
            result.rust_deduplicate_cached_modified,
            result.upstream_deduplicate_cached_modified
        );
        assert_eq!(
            result.rust_stage_others_deleted_modified,
            result.upstream_stage_others_deleted_modified
        );
    }

    #[test]
    fn upstream_git_observes_rust_update_ref_delete() {
        let result = update_ref_delete_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert!(result.before.contains("refs/heads/topic"));
        assert!(!result.after.contains("refs/heads/topic"));
        assert_eq!(result.deleted_oid.len(), result.format.hex_len());
    }

    #[test]
    fn upstream_git_observes_rust_update_ref_delete_sha256() {
        let result = update_ref_delete_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert!(result.before.contains("refs/heads/topic"));
        assert!(!result.after.contains("refs/heads/topic"));
        assert_eq!(result.deleted_oid.len(), result.format.hex_len());
    }

    #[test]
    fn upstream_git_observes_rust_update_ref_delete_packed() {
        let result = update_ref_delete_packed_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert!(result.before.contains("refs/heads/topic"));
        assert!(!result.after.contains("refs/heads/topic"));
        assert_eq!(result.deleted_oid.len(), result.format.hex_len());
    }

    #[test]
    fn upstream_git_observes_rust_update_ref_delete_packed_sha256() {
        let result =
            update_ref_delete_packed_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert!(result.before.contains("refs/heads/topic"));
        assert!(!result.after.contains("refs/heads/topic"));
        assert_eq!(result.deleted_oid.len(), result.format.hex_len());
    }

    #[test]
    fn upstream_git_observes_rust_reflog_expire() {
        let result = reflog_expire_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.removed, 1);
        assert_eq!(result.before, "commit: second\ncommit: first\n");
        assert_eq!(result.after, "commit: second\n");
    }

    #[test]
    fn upstream_git_observes_rust_reflog_expire_sha256() {
        let result = reflog_expire_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.removed, 1);
        assert_eq!(result.before, "commit: second\ncommit: first\n");
        assert_eq!(result.after, "commit: second\n");
    }

    #[test]
    fn rust_write_tree_matches_upstream_git() {
        let result = write_tree_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.rust, result.upstream);
    }

    #[test]
    fn rust_write_tree_matches_upstream_git_sha256() {
        let result = write_tree_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.rust, result.upstream);
    }

    #[test]
    fn rust_commit_tree_matches_upstream_git() {
        let result = commit_tree_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.rust, result.upstream);
        assert!(String::from_utf8_lossy(&result.body).contains("initial subject"));
    }

    #[test]
    fn rust_commit_tree_matches_upstream_git_sha256() {
        let result = commit_tree_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.rust, result.upstream);
        assert!(String::from_utf8_lossy(&result.body).contains("initial subject"));
    }

    #[test]
    fn upstream_git_reads_rust_commit_index_result() {
        let result = commit_index_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.updated_ref, "refs/heads/main");
        assert!(result.log.contains("initial subject"));
    }

    #[test]
    fn upstream_git_reads_rust_commit_index_result_sha256() {
        let result = commit_index_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.updated_ref, "refs/heads/main");
        assert!(result.log.contains("initial subject"));
    }

    #[test]
    fn rust_add_status_matches_upstream_git_short_status() {
        let result = add_status_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_add_status_parity(result);
    }

    #[test]
    fn rust_add_status_matches_upstream_git_short_status_sha256() {
        let result = add_status_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_add_status_parity(result);
    }

    fn assert_add_status_parity(result: AddStatusParity) {
        assert_eq!(result.rust, result.upstream);
        assert_eq!(result.porcelain_rust, result.porcelain_upstream);
        assert_eq!(
            result.porcelain_branch_rust,
            result.porcelain_branch_upstream
        );
        assert_eq!(result.porcelain_z_rust, result.porcelain_z_upstream);
        assert_eq!(
            result.porcelain_branch_z_rust,
            result.porcelain_branch_z_upstream
        );
    }

    #[test]
    fn upstream_git_lists_rust_created_branch() {
        let result = branch_create_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_branch_create_parity(result);
    }

    #[test]
    fn upstream_git_lists_rust_created_branch_sha256() {
        let result = branch_create_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_branch_create_parity(result);
    }

    fn assert_branch_create_parity(result: BranchParity) {
        assert_eq!(result.upstream, result.expected);
        assert_eq!(result.remotes_upstream, result.remotes_expected);
        assert_eq!(result.all_upstream, result.all_expected);
        assert_eq!(result.points_at_upstream, result.points_at_expected);
        assert_eq!(result.points_at_oid_upstream, result.points_at_oid_expected);
    }

    #[test]
    fn rust_branch_show_current_matches_upstream_git() {
        let result = branch_show_current_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.rust, result.upstream);
    }

    #[test]
    fn rust_branch_show_current_matches_upstream_git_sha256() {
        let result = branch_show_current_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.rust, result.upstream);
    }

    #[test]
    fn upstream_git_observes_rust_deleted_branch() {
        let result = branch_delete_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_branch_delete_parity(result);
    }

    #[test]
    fn upstream_git_observes_rust_deleted_branch_sha256() {
        let result = branch_delete_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_branch_delete_parity(result);
    }

    fn assert_branch_delete_parity(result: BranchDeleteParity) {
        assert_eq!(result.before, "  feature\n* main\n");
        assert_eq!(result.after, "* main\n");
        assert_eq!(result.deleted_oid.len(), result.format.hex_len());
    }

    #[test]
    fn upstream_git_reads_rust_checkout_branch_result() {
        let result = checkout_branch_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_checkout_branch_parity(result);
    }

    #[test]
    fn upstream_git_reads_rust_checkout_branch_result_sha256() {
        let result = checkout_branch_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_checkout_branch_parity(result);
    }

    fn assert_checkout_branch_parity(result: CheckoutParity) {
        assert_eq!(result.branch, "feature");
        assert_eq!(result.head.len(), result.format.hex_len());
        assert_eq!(result.body, b"feature\n");
        assert!(result.status.is_empty());
    }

    #[test]
    fn upstream_git_lists_rust_created_tag() {
        let result = tag_create_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_tag_create_parity(result);
    }

    #[test]
    fn upstream_git_lists_rust_created_tag_sha256() {
        let result = tag_create_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_tag_create_parity(result);
    }

    fn assert_tag_create_parity(result: TagParity) {
        assert_eq!(result.upstream, result.expected);
        assert!(result.show_ref.contains("refs/tags/v1.0"));
    }

    #[test]
    fn upstream_git_observes_rust_deleted_tag() {
        let result = tag_delete_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_tag_delete_parity(result);
    }

    #[test]
    fn upstream_git_observes_rust_deleted_tag_sha256() {
        let result = tag_delete_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_tag_delete_parity(result);
    }

    fn assert_tag_delete_parity(result: TagDeleteParity) {
        assert_eq!(result.before, "v1.0\n");
        assert!(result.after.is_empty());
        assert_eq!(result.deleted_oid.len(), result.format.hex_len());
    }

    #[test]
    fn upstream_git_reads_rust_created_annotated_tag() {
        let result = annotated_tag_create_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_annotated_tag_create_parity(result);
    }

    #[test]
    fn upstream_git_reads_rust_created_annotated_tag_sha256() {
        let result = annotated_tag_create_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_annotated_tag_create_parity(result);
    }

    fn assert_annotated_tag_create_parity(result: AnnotatedTagParity) {
        assert_eq!(result.upstream_type, "tag");
        assert_eq!(result.tag_oid.len(), result.format.hex_len());
        assert_eq!(result.target_oid.len(), result.format.hex_len());
        assert_eq!(result.upstream_body, result.expected_body);
        assert!(result.show_ref.contains("refs/tags/v2.0"));
    }

    #[test]
    fn rust_diff_name_status_matches_upstream_git() {
        let result = diff_name_status_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_diff_name_status_parity(result);
    }

    #[test]
    fn rust_diff_name_status_matches_upstream_git_sha256() {
        let result = diff_name_status_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_diff_name_status_parity(result);
    }

    fn assert_diff_name_status_parity(result: DiffNameStatusParity) {
        assert_eq!(result.rust, result.upstream);
        assert_eq!(result.name_only_rust, result.name_only_upstream);
        assert_eq!(result.cached_rust, result.cached_upstream);
        assert_eq!(
            result.cached_name_only_rust,
            result.cached_name_only_upstream
        );
        assert_eq!(result.rename_copy_rust, result.rename_copy_upstream);
        assert_eq!(
            result.rename_copy_name_only_rust,
            result.rename_copy_name_only_upstream
        );
    }

    #[test]
    fn rust_rev_parse_matches_upstream_git() {
        let result = rev_parse_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_rev_parse_parity(result);
    }

    #[test]
    fn rust_rev_parse_matches_upstream_git_sha256() {
        let result = rev_parse_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_rev_parse_parity(result);
    }

    fn assert_rev_parse_parity(result: RevParseParity) {
        assert_eq!(result.rust, result.upstream);
        assert_eq!(result.short_rust, result.short_upstream);
        assert_eq!(result.short_8_rust, result.short_8_upstream);
        assert_eq!(result.short_min_rust, result.short_min_upstream);
        assert_eq!(result.verify_rust, result.verify_upstream);
        assert_eq!(result.verify_quiet_rust, result.verify_quiet_upstream);
        assert_eq!(result.verify_short_rust, result.verify_short_upstream);
        assert_eq!(result.abbrev_ref_rust, result.abbrev_ref_upstream);
        assert_eq!(
            result.symbolic_full_name_rust,
            result.symbolic_full_name_upstream
        );
        assert_eq!(result.top_level_rust, result.top_level_upstream);
        assert_eq!(result.prefix_root_rust, result.prefix_root_upstream);
        assert_eq!(result.prefix_nested_rust, result.prefix_nested_upstream);
        assert_eq!(result.cdup_root_rust, result.cdup_root_upstream);
        assert_eq!(result.cdup_nested_rust, result.cdup_nested_upstream);
        assert_eq!(result.git_dir_rust, result.git_dir_upstream);
        assert_eq!(
            result.absolute_git_dir_rust,
            result.absolute_git_dir_upstream
        );
        assert_eq!(
            result.inside_work_tree_rust,
            result.inside_work_tree_upstream
        );
        assert_eq!(
            result.inside_git_dir_worktree_rust,
            result.inside_git_dir_worktree_upstream
        );
        assert_eq!(
            result.inside_git_dir_git_rust,
            result.inside_git_dir_git_upstream
        );
        assert_eq!(
            result.inside_git_dir_bare_rust,
            result.inside_git_dir_bare_upstream
        );
        assert_eq!(result.bare_worktree_rust, result.bare_worktree_upstream);
        assert_eq!(result.bare_repo_rust, result.bare_repo_upstream);
        assert_eq!(
            result.shallow_worktree_rust,
            result.shallow_worktree_upstream
        );
        assert_eq!(result.shallow_marker_rust, result.shallow_marker_upstream);
        assert_eq!(result.shallow_bare_rust, result.shallow_bare_upstream);
    }

    #[test]
    fn rust_rev_parse_object_format_matches_upstream_git() {
        let result = rev_parse_object_format_parity().expect("test operation should succeed");
        assert_eq!(result.sha1_rust, result.sha1_upstream);
        assert_eq!(result.sha256_rust, result.sha256_upstream);
    }

    #[test]
    fn rust_rev_parse_parent_syntax_matches_upstream_git() {
        let result = rev_parse_parent_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.rust, result.upstream);
    }

    #[test]
    fn rust_rev_parse_parent_syntax_matches_upstream_git_sha256() {
        let result = rev_parse_parent_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.rust, result.upstream);
    }

    #[test]
    fn rust_rev_parse_peel_syntax_matches_upstream_git() {
        let result = rev_parse_peel_parity().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha1);
        assert_eq!(result.rust, result.upstream);
    }

    #[test]
    fn rust_rev_parse_peel_syntax_matches_upstream_git_sha256() {
        let result = rev_parse_peel_parity_sha256().expect("test operation should succeed");
        assert_eq!(result.format, ObjectFormat::Sha256);
        assert_eq!(result.rust, result.upstream);
    }

    // --- upstream module helpers (pure; no git required) ------------------

    #[test]
    fn upstream_command_script_mapping_is_bijective() {
        use crate::upstream::{
            DEFAULT_SCRIPTS, FOUNDATIONAL_COMMANDS, command_for_script, script_for_command,
        };
        // Every foundational command resolves to a script and back.
        for (name, script) in FOUNDATIONAL_COMMANDS {
            assert_eq!(script_for_command(name), Some(*script));
            assert_eq!(command_for_script(script), Some(*name));
        }
        // The command map and the DEFAULT_SCRIPTS list cover the same scripts.
        let mut mapped: Vec<&str> = FOUNDATIONAL_COMMANDS.iter().map(|(_, s)| *s).collect();
        mapped.sort_unstable();
        let mut defaults: Vec<&str> = DEFAULT_SCRIPTS.to_vec();
        defaults.sort_unstable();
        assert_eq!(mapped, defaults);
        // Unknown names resolve to None.
        assert_eq!(script_for_command("definitely-not-a-command"), None);
        assert_eq!(command_for_script("t9999-nope.sh"), None);
    }

    #[test]
    fn upstream_parse_results_extracts_command_and_counts() {
        use crate::upstream::parse_results;
        // A representative slice of the runner's stdout table.
        let stdout = "\
SCRIPT                       RESULT      OK  FAIL  DETAIL
-------------------------------------------------------------------------
t1300-config.sh              FAIL       131   367  rc=1 (1..498)
t3103-ls-tree-misc.sh        TIMEOUT      1     9  exceeded 120s
t9999-custom.sh              PASS       10     0  1..10
not-a-row should be ignored
";
        let results = parse_results(stdout);
        assert_eq!(results.len(), 3);

        let cfg = &results[0];
        assert_eq!(cfg.script, "t1300-config.sh");
        assert_eq!(cfg.command, "config");
        assert_eq!(cfg.result, "FAIL");
        assert_eq!(cfg.ok, 131);
        assert_eq!(cfg.failed, 367);
        assert_eq!(cfg.total(), 498);
        assert_eq!(cfg.pass_rate(), 26); // 131*100/498

        // A non-foundational script falls back to its basename for `command`.
        let custom = &results[2];
        assert_eq!(custom.command, "t9999-custom.sh");
        assert_eq!(custom.pass_rate(), 100);
    }

    #[test]
    fn upstream_pass_rate_handles_zero_total() {
        use crate::upstream::ScriptResult;
        let empty = ScriptResult {
            script: "t0000.sh".into(),
            command: "t0000.sh".into(),
            result: "TIMEOUT".into(),
            ok: 0,
            failed: 0,
        };
        assert_eq!(empty.total(), 0);
        assert_eq!(empty.pass_rate(), 0);
    }
}
