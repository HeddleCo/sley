# Sley 0.12.1 release notes

0.12.1 is an additive security patch release. It contains #248 and #250 and
changes no public signatures. The 0.12 migration notes in
[SLEY_0.12_MIGRATION.md](SLEY_0.12_MIGRATION.md) remain the reference for the
0.12 breaking changes.

## Checkout path safety

- Checkout paths are verified with git's `verify_path` rules before any write.
  Entries that name `.git` through aliases are refused.
- NTFS and HFS alias protection (`core.protectNTFS`, `core.protectHFS`) now
  default to `true` on every platform, including HFS outside macOS.
- read-tree no longer defaults either protection off. Repository configuration
  can still disable these alias checks.
- On Windows, backslashes, drive prefixes and rooted tree names are rejected.
  NTFS protection also rejects reserved device names, invalid characters and
  trailing dots or spaces.

## No-follow worktree writes

- Tracked-file removals and empty-directory pruning use directory handles and
  never follow symlinked parents.
- The path-only `write_blob_body_or_symlink` helper refuses symlinks in parents
  or at the leaf. It still overwrites existing regular files.
- Reset skips case collisions the same way checkout does.

## `WorktreePathPolicy`

`sley_worktree::WorktreePathPolicy` carries the path rules explicitly:

- `protect_ntfs(bool)` and `protect_hfs(bool)` set the alias protections.
- `from_config(&GitConfig)` derives them from repository configuration.
- `reserve_root_name` and `reserve_name` add names that may not be written.
- `verify_path(path, mode)` returns an error for a rejected path.

The `_with_path_policy` entry points pass the policy into parallel checkout and
delayed filter writes. Unpack callers can use
`ReadTreeWorktree::with_path_policy` without changing existing struct literals.
