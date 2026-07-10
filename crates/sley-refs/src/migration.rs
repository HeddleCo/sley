//! Backend-independent reference storage migration.
//!
//! This module owns the semantic operation behind `git refs migrate`: it
//! snapshots the active reference backend, materializes the target backend in
//! an isolated directory, and either returns that dry-run artifact or installs
//! it into the repository. Callers retain responsibility for parsing options
//! and rendering user-facing diagnostics.

use crate::{FileRefStore, Ref, ReflogEntry};
use sley_config::{ConfigEntry, ConfigSection, GitConfig};
use sley_core::{GitError, ObjectFormat};
use sley_formats::RefStorageFormat;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Fully resolved inputs for a reference storage migration.
#[derive(Debug, Clone, Copy)]
pub struct MigrateRefStorageOptions<'a> {
    /// Worktree-specific repository directory containing `HEAD` and root refs.
    pub git_dir: &'a Path,
    /// Shared repository directory containing refs, logs, and repository config.
    pub common_git_dir: &'a Path,
    pub object_format: ObjectFormat,
    pub target_format: RefStorageFormat,
    pub include_reflogs: bool,
    /// Materialize the target store but do not install it.
    pub dry_run: bool,
}

/// Observable result of a completed storage migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateRefStorageOutcome {
    pub source_format: RefStorageFormat,
    pub target_format: RefStorageFormat,
    pub refs_migrated: usize,
    pub reflogs_migrated: usize,
    /// The retained migration directory for a dry run; installed migrations
    /// clean up their staging directory and return `None`.
    pub dry_run_path: Option<PathBuf>,
}

/// Semantic failures callers may render according to their frontend.
#[derive(Debug)]
pub enum MigrateRefStorageError {
    AlreadyUses(RefStorageFormat),
    LinkedWorktreesUnsupported,
    Storage(GitError),
}

impl fmt::Display for MigrateRefStorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyUses(format) => {
                write!(f, "repository already uses '{}' format", format.name())
            }
            Self::LinkedWorktreesUnsupported => {
                f.write_str("migrating repositories with worktrees is not supported yet")
            }
            Self::Storage(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for MigrateRefStorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(err) => Some(err),
            Self::AlreadyUses(_) | Self::LinkedWorktreesUnsupported => None,
        }
    }
}

impl From<GitError> for MigrateRefStorageError {
    fn from(value: GitError) -> Self {
        Self::Storage(value)
    }
}

impl From<io::Error> for MigrateRefStorageError {
    fn from(value: io::Error) -> Self {
        Self::Storage(value.into())
    }
}

/// Migrate all live refs and, optionally, reflogs to another storage backend.
///
/// The destination is always built completely in a repository-local staging
/// directory before the active backend is touched. A dry run returns that
/// directory for inspection. The operation never invokes another executable.
pub fn migrate_ref_storage(
    options: MigrateRefStorageOptions<'_>,
) -> Result<MigrateRefStorageOutcome, MigrateRefStorageError> {
    let source_format = current_ref_storage_format(options.common_git_dir)?;
    if source_format == options.target_format {
        return Err(MigrateRefStorageError::AlreadyUses(source_format));
    }
    if has_linked_worktrees(options.common_git_dir) {
        return Err(MigrateRefStorageError::LinkedWorktreesUnsupported);
    }

    let source = FileRefStore::new(options.git_dir, options.object_format);
    let snapshot = source.export_snapshot(options.include_reflogs)?;
    let refs_migrated = snapshot.refs.len();
    let reflogs_migrated = snapshot.reflogs.len();

    let migration_dir = create_migration_dir(options.common_git_dir)?;
    write_migrated_ref_store(
        &migration_dir,
        options.object_format,
        options.target_format,
        &snapshot.refs,
        &snapshot.reflogs,
    )?;
    if options.dry_run {
        return Ok(MigrateRefStorageOutcome {
            source_format,
            target_format: options.target_format,
            refs_migrated,
            reflogs_migrated,
            dry_run_path: Some(migration_dir),
        });
    }

    install_migrated_ref_store(
        options.git_dir,
        options.common_git_dir,
        &migration_dir,
        options.target_format,
        &snapshot.refs,
    )?;
    update_ref_storage_config(options.common_git_dir, options.target_format)?;
    let _ = fs::remove_dir_all(&migration_dir);

    Ok(MigrateRefStorageOutcome {
        source_format,
        target_format: options.target_format,
        refs_migrated,
        reflogs_migrated,
        dry_run_path: None,
    })
}

/// Detect the repository's active shared reference backend.
pub fn current_ref_storage_format(
    common_git_dir: &Path,
) -> Result<RefStorageFormat, MigrateRefStorageError> {
    let Ok(config) = GitConfig::read(common_git_dir.join("config")) else {
        return Ok(RefStorageFormat::Files);
    };
    Ok(match config.get("extensions", None, "refStorage") {
        Some("reftable") => RefStorageFormat::Reftable,
        Some(value) if value.starts_with("reftable://") => RefStorageFormat::Reftable,
        _ => RefStorageFormat::Files,
    })
}

fn has_linked_worktrees(common_git_dir: &Path) -> bool {
    fs::read_dir(common_git_dir.join("worktrees"))
        .map(|mut entries| entries.any(|entry| entry.is_ok()))
        .unwrap_or(false)
}

fn create_migration_dir(common_git_dir: &Path) -> Result<PathBuf, MigrateRefStorageError> {
    for attempt in 0..1000u32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = common_git_dir.join(format!(
            "ref_migration.{:x}{:x}",
            std::process::id(),
            nanos + u128::from(attempt)
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(MigrateRefStorageError::Storage(GitError::Io(
        "unable to create temporary ref migration directory".into(),
    )))
}

fn write_migrated_ref_store(
    migration_dir: &Path,
    object_format: ObjectFormat,
    target_format: RefStorageFormat,
    refs: &[Ref],
    reflogs: &[(String, Vec<ReflogEntry>)],
) -> Result<(), MigrateRefStorageError> {
    if target_format == RefStorageFormat::Reftable {
        write_ref_migration_config(migration_dir)?;
    } else {
        fs::write(
            migration_dir.join("HEAD"),
            b"ref: refs/heads/__sley_migration_placeholder__\n",
        )?;
    }
    let store = FileRefStore::new(migration_dir, object_format);
    store.import_snapshot(refs, reflogs, target_format == RefStorageFormat::Files)?;
    Ok(())
}

fn write_ref_migration_config(git_dir: &Path) -> Result<(), MigrateRefStorageError> {
    let mut config = GitConfig::default();
    set_config_value(&mut config, "core", "repositoryformatversion", "1");
    set_config_value(&mut config, "extensions", "refStorage", "reftable");
    fs::write(git_dir.join("config"), config.to_canonical_bytes())?;
    Ok(())
}

fn install_migrated_ref_store(
    git_dir: &Path,
    common_git_dir: &Path,
    migration_dir: &Path,
    target_format: RefStorageFormat,
    refs: &[Ref],
) -> Result<(), MigrateRefStorageError> {
    match target_format {
        RefStorageFormat::Files => {
            remove_path_if_exists(&common_git_dir.join("reftable"))?;
            remove_path_if_exists(&common_git_dir.join("refs"))?;
            remove_path_if_exists(&common_git_dir.join("logs"))?;
            remove_path_if_exists(&common_git_dir.join("packed-refs"))?;
            move_path_if_exists(&migration_dir.join("HEAD"), &git_dir.join("HEAD"))?;
            for reference in refs {
                if !reference.name.starts_with("refs/") && reference.name != "HEAD" {
                    move_path_if_exists(
                        &migration_dir.join(&reference.name),
                        &git_dir.join(&reference.name),
                    )?;
                }
            }
            move_path_if_exists(&migration_dir.join("refs"), &common_git_dir.join("refs"))?;
            fs::create_dir_all(common_git_dir.join("refs").join("heads"))?;
            fs::create_dir_all(common_git_dir.join("refs").join("tags"))?;
            move_path_if_exists(&migration_dir.join("logs"), &common_git_dir.join("logs"))?;
            move_path_if_exists(
                &migration_dir.join("packed-refs"),
                &common_git_dir.join("packed-refs"),
            )?;
        }
        RefStorageFormat::Reftable => {
            remove_path_if_exists(&common_git_dir.join("reftable"))?;
            remove_path_if_exists(&common_git_dir.join("refs"))?;
            remove_path_if_exists(&common_git_dir.join("logs"))?;
            remove_path_if_exists(&common_git_dir.join("packed-refs"))?;
            for reference in refs {
                if !reference.name.starts_with("refs/")
                    && reference.name != "HEAD"
                    && reference.name != "FETCH_HEAD"
                    && reference.name != "MERGE_HEAD"
                {
                    remove_path_if_exists(&git_dir.join(&reference.name))?;
                }
            }
            move_path_if_exists(
                &migration_dir.join("reftable"),
                &common_git_dir.join("reftable"),
            )?;
            fs::write(git_dir.join("HEAD"), b"ref: refs/heads/.invalid\n")?;
            fs::create_dir_all(common_git_dir.join("refs"))?;
            fs::write(
                common_git_dir.join("refs").join("heads"),
                b"this repository uses the reftable format\n",
            )?;
        }
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> Result<(), MigrateRefStorageError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

fn move_path_if_exists(from: &Path, to: &Path) -> Result<(), MigrateRefStorageError> {
    if !from.exists() {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    remove_path_if_exists(to)?;
    fs::rename(from, to)?;
    Ok(())
}

fn update_ref_storage_config(
    common_git_dir: &Path,
    target_format: RefStorageFormat,
) -> Result<(), MigrateRefStorageError> {
    let config_path = common_git_dir.join("config");
    let mut config = GitConfig::read(&config_path).unwrap_or_default();
    set_config_value(&mut config, "core", "repositoryformatversion", "1");
    set_config_value(
        &mut config,
        "extensions",
        "refStorage",
        target_format.name(),
    );
    fs::write(config_path, config.to_canonical_bytes())?;
    Ok(())
}

fn set_config_value(config: &mut GitConfig, section_name: &str, key_name: &str, value: &str) {
    let section_idx = config
        .sections
        .iter()
        .rposition(|section| section.name.eq_ignore_ascii_case(section_name))
        .unwrap_or_else(|| {
            config.sections.push(ConfigSection::new(
                section_name.to_string(),
                None,
                Vec::new(),
            ));
            config.sections.len() - 1
        });
    let section = &mut config.sections[section_idx];
    if let Some(entry) = section
        .entries
        .iter_mut()
        .find(|entry| entry.key.eq_ignore_ascii_case(key_name))
    {
        entry.key = key_name.to_string();
        entry.value = Some(value.to_string());
        return;
    }
    section.entries.push(ConfigEntry::new(
        key_name.to_string(),
        Some(value.to_string()),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RefTarget, ReflogEntry};
    use sley_core::ObjectId;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn files_to_reftable_dry_run_is_non_destructive() {
        let git_dir = temp_git_dir();
        seed_files_repository(&git_dir, ObjectFormat::Sha1);

        let outcome = migrate_ref_storage(MigrateRefStorageOptions {
            git_dir: &git_dir,
            common_git_dir: &git_dir,
            object_format: ObjectFormat::Sha1,
            target_format: RefStorageFormat::Reftable,
            include_reflogs: true,
            dry_run: true,
        })
        .expect("dry-run migration");

        let staged = outcome.dry_run_path.expect("retained staging directory");
        assert_eq!(outcome.source_format, RefStorageFormat::Files);
        assert_eq!(outcome.refs_migrated, 2);
        assert_eq!(outcome.reflogs_migrated, 1);
        assert_eq!(
            FileRefStore::new(&git_dir, ObjectFormat::Sha1)
                .read_ref("refs/heads/main")
                .expect("read source"),
            Some(RefTarget::Direct(test_oid(ObjectFormat::Sha1)))
        );
        assert_eq!(
            FileRefStore::new(&staged, ObjectFormat::Sha1)
                .read_ref("refs/heads/main")
                .expect("read staged target"),
            Some(RefTarget::Direct(test_oid(ObjectFormat::Sha1)))
        );

        fs::remove_dir_all(git_dir).expect("remove test repository");
    }

    #[test]
    fn migration_round_trip_preserves_refs_and_reflogs() {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let git_dir = temp_git_dir();
            seed_files_repository(&git_dir, format);
            let source = FileRefStore::new(&git_dir, format)
                .export_snapshot(true)
                .expect("export source");

            let to_reftable = migrate_ref_storage(MigrateRefStorageOptions {
                git_dir: &git_dir,
                common_git_dir: &git_dir,
                object_format: format,
                target_format: RefStorageFormat::Reftable,
                include_reflogs: true,
                dry_run: false,
            })
            .expect("migrate to reftable");
            assert_eq!(to_reftable.source_format, RefStorageFormat::Files);
            assert!(to_reftable.dry_run_path.is_none());
            assert_eq!(
                FileRefStore::new(&git_dir, format)
                    .export_snapshot(true)
                    .expect("export reftable"),
                source
            );

            migrate_ref_storage(MigrateRefStorageOptions {
                git_dir: &git_dir,
                common_git_dir: &git_dir,
                object_format: format,
                target_format: RefStorageFormat::Files,
                include_reflogs: true,
                dry_run: false,
            })
            .expect("migrate back to files");
            assert_eq!(
                FileRefStore::new(&git_dir, format)
                    .export_snapshot(true)
                    .expect("export files"),
                source
            );

            fs::remove_dir_all(git_dir).expect("remove test repository");
        }
    }

    #[test]
    fn migration_rejects_same_backend_and_linked_worktrees() {
        let git_dir = temp_git_dir();
        seed_files_repository(&git_dir, ObjectFormat::Sha1);
        let same = migrate_ref_storage(MigrateRefStorageOptions {
            git_dir: &git_dir,
            common_git_dir: &git_dir,
            object_format: ObjectFormat::Sha1,
            target_format: RefStorageFormat::Files,
            include_reflogs: true,
            dry_run: false,
        });
        assert!(matches!(
            same,
            Err(MigrateRefStorageError::AlreadyUses(RefStorageFormat::Files))
        ));

        fs::create_dir_all(git_dir.join("worktrees").join("linked"))
            .expect("create linked worktree marker");
        let linked = migrate_ref_storage(MigrateRefStorageOptions {
            git_dir: &git_dir,
            common_git_dir: &git_dir,
            object_format: ObjectFormat::Sha1,
            target_format: RefStorageFormat::Reftable,
            include_reflogs: true,
            dry_run: false,
        });
        assert!(matches!(
            linked,
            Err(MigrateRefStorageError::LinkedWorktreesUnsupported)
        ));

        fs::remove_dir_all(git_dir).expect("remove test repository");
    }

    fn seed_files_repository(git_dir: &Path, format: ObjectFormat) {
        fs::write(
            git_dir.join("config"),
            b"[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("write config");
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("write HEAD");
        let store = FileRefStore::new(git_dir, format);
        store
            .import_snapshot(
                &[
                    Ref {
                        name: "HEAD".into(),
                        target: RefTarget::Symbolic("refs/heads/main".into()),
                    },
                    Ref {
                        name: "refs/heads/main".into(),
                        target: RefTarget::Direct(test_oid(format)),
                    },
                ],
                &[(
                    "refs/heads/main".into(),
                    vec![ReflogEntry {
                        old_oid: ObjectId::null(format),
                        new_oid: test_oid(format),
                        committer: b"Sley <sley@example.invalid> 1 +0000".to_vec(),
                        message: b"seed".to_vec(),
                    }],
                )],
                true,
            )
            .expect("seed ref store");
    }

    fn test_oid(format: ObjectFormat) -> ObjectId {
        ObjectId::from_hex(format, &"1".repeat(format.hex_len())).expect("valid test oid")
    }

    fn temp_git_dir() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "sley-refs-migration-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create test repository");
        path
    }
}
