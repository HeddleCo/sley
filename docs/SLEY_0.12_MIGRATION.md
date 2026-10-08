# Sley 0.12 migration

0.12 is a breaking release. Ref-name validation now matches Git exactly (#245).

## `FullName` is stricter

`FullName` now follows `git check-ref-format`. Names that 0.11 accepted are
rejected if they contain:

- `..`
- a component ending in `.lock`
- any of `~ ^ : ? * [ \`
- `@{`
- a lone `@`
- a trailing `.`

Validate untrusted input with `FullName` before you create or update refs, and
handle the new `Err` cases.

## Deletion takes `DeleteRefName`

Ref deletion now takes `sley::DeleteRefName`. It is validated by
`sley_refs::refname_is_safe`, Git's delete-time rule, rather than by
`check-ref-format`. As with `git update-ref -d`, this lets you remove a
malformed ref that Git would refuse to create (for example
`refs/heads/broken...ref`). Build one with `DeleteRefName::new(name)?`.

## `find_reference` rejects malformed names

`Repository::find_reference` now returns `Err` for a malformed name instead of
`Ok(None)`. `Ok(None)` still means a well-formed ref that does not exist.

## 0.12.1 worktree path safety

Checkout, reset, read-tree and merge writers default both `core.protectNTFS`
and `core.protectHFS` to `true` on every platform. HFS protection now defaults
on outside macOS too; read-tree no longer defaults either protection off.
Repository configuration can disable these alias checks. Windows still rejects
backslashes, drive prefixes and rooted tree names; its NTFS protection also
rejects reserved device names, invalid characters and trailing dots/spaces.

The `_with_path_policy` entry points carry the caller's policy explicitly into
parallel checkout and delayed filter writes. Unpack callers can use
`ReadTreeWorktree::with_path_policy` without changing existing struct literals.
Tracked-file removals and empty-directory pruning use directory handles and
never follow symlink parents; reset skips case collisions like checkout.

Public signatures remain compatible. The path-only
`write_blob_body_or_symlink` helper now refuses symlinks in parents or at the
leaf and still overwrites existing regular files. No API removal or version
bump is required for this patch release.
