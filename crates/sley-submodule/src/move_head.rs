//! Submodule move-head / verify-clean primitives — a Rust port of the
//! verification half of git's `submodule.c::submodule_move_head` plus the
//! `unpack-trees.c` wrappers `check_submodule_move_head` and
//! `verify_clean_submodule`.
//!
//! These are exactly the hooks a tree-switch engine (checkout / reset /
//! read-tree, i.e. `unpack-trees`) needs to answer "would moving this
//! submodule's HEAD from `old` to `new` lose uncommitted work?" *before*
//! touching the worktree. They are written as pure decision logic over a
//! caller-supplied [`MoveHeadContext`]: the crate owns the algorithm (the
//! active/populated gating and the dirty-index → reject rule); the caller owns
//! the I/O (resolving the submodule gitdir, reading its index, checking
//! dirtiness). That split keeps this crate dependency-light and lets both the
//! `submodule` CLI command and the tree-switch commands drive the SAME logic.
//!
//! ## Mapping to git
//!
//! | git symbol | here |
//! |---|---|
//! | `submodule_move_head(..., DRY_RUN)` | [`check_move_head`] |
//! | `check_submodule_move_head` (unpack-trees.c) | [`check_move_head`] |
//! | `verify_clean_submodule` (unpack-trees.c) | [`verify_clean_submodule`] |
//! | `SUBMODULE_MOVE_HEAD_FORCE` | [`MoveHeadFlags::force`] |
//! | `is_submodule_active` | [`MoveHeadContext::active`] |
//! | `is_submodule_populated_gently` | [`MoveHeadContext::populated`] |
//! | `submodule_has_dirty_index` | [`MoveHeadContext::has_dirty_index`] |

/// Flags controlling a move-head check, porting `SUBMODULE_MOVE_HEAD_*`.
///
/// Note: only the verification (dry-run) decision is modeled here, which is all
/// `check_submodule_move_head` ever needs (it always sets `DRY_RUN`). The
/// non-dry-run mutation path of `submodule_move_head` (absorb gitdir, run
/// `read-tree -u`, update HEAD) is a CLI/engine concern and lives with the
/// caller — see TODO(submodule) below.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MoveHeadFlags {
    /// `SUBMODULE_MOVE_HEAD_FORCE` — set on a hard `reset`. When forced, a dirty
    /// index does NOT block the move (git skips the dirty-index check).
    pub force: bool,
}

/// Everything the move-head decision needs about one submodule at one path.
/// The caller resolves these via whatever I/O it already has (the CLI reads the
/// submodule's `.git` + index; the unpack-trees engine reads the in-memory
/// index entry + on-disk submodule).
pub struct MoveHeadContext {
    /// `is_submodule_active(the_repository, path)` — is the submodule active in
    /// this superproject (via `submodule.<name>.active` / `active`-pathspec /
    /// a configured url)? An inactive submodule is never touched, so a move can
    /// never lose its data.
    pub active: bool,
    /// `is_submodule_populated_gently(path, ...)` — does `<path>/.git` resolve
    /// to a real git repository? An unpopulated submodule has no checked-out
    /// work to lose when `old_head` is set.
    pub populated: bool,
    /// `submodule_has_dirty_index(sub)` — does the submodule have staged but
    /// uncommitted changes relative to its HEAD (`git diff-index --quiet
    /// --cached HEAD` is non-zero)? A dirty index blocks a non-forced move.
    pub has_dirty_index: bool,
}

/// The verdict of a move-head check, porting the return contract of
/// `submodule_move_head` in DRY_RUN mode as consumed by
/// `check_submodule_move_head`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveHeadVerdict {
    /// The move is safe: no submodule work would be lost. (git returns 0.)
    Ok,
    /// The move would lose submodule work and must be rejected — the
    /// unpack-trees caller records this as `ERROR_WOULD_LOSE_SUBMODULE` for
    /// `ce->name`. (git returns non-zero from `submodule_move_head`.)
    WouldLose,
}

/// Port of `submodule_move_head` (git `submodule.c`) restricted to the dry-run
/// verification path that `check_submodule_move_head` uses — the actual hook a
/// tree-switch engine calls.
///
/// `old_head` is `None` when the submodule is *coming into existence* in the
/// new tree (git passes `NULL` for old); `new_head` is `None` when it is being
/// *removed*. The decision:
///
/// 1. Inactive submodule → [`MoveHeadVerdict::Ok`] (git's early `return 0`).
/// 2. `old_head` set but the submodule is not populated → `Ok` (nothing to
///    lose; git's `return 0`).
/// 3. `old_head` set, not forced, and the index is dirty → [`MoveHeadVerdict::WouldLose`]
///    (git's `error("submodule '%s' has dirty index")`).
/// 4. Otherwise the dry-run `read-tree -n -m` would succeed → `Ok`.
///
/// Step 4's `read-tree -n` (a no-op-checking merge) can itself reject when the
/// new tree would clobber tracked-but-modified worktree files. That nested
/// check is left to the caller's worktree-status pass — see TODO(submodule).
pub fn check_move_head(
    ctx: &MoveHeadContext,
    old_head: Option<&str>,
    _new_head: Option<&str>,
    flags: MoveHeadFlags,
) -> MoveHeadVerdict {
    // 1. `if (!is_submodule_active(the_repository, path)) return 0;`
    if !ctx.active {
        return MoveHeadVerdict::Ok;
    }

    // 2. `if (old_head && !is_submodule_populated_gently(path, ...)) return 0;`
    if old_head.is_some() && !ctx.populated {
        return MoveHeadVerdict::Ok;
    }

    // 3. `if (old_head && !FORCE) { if (submodule_has_dirty_index(sub)) error(...); }`
    if old_head.is_some() && !flags.force && ctx.has_dirty_index {
        return MoveHeadVerdict::WouldLose;
    }

    // 4. The dry-run read-tree succeeds (nested worktree-clobber check is the
    //    caller's; see module docs). git returns 0.
    MoveHeadVerdict::Ok
}

/// Port of `check_submodule_move_head` (git `unpack-trees.c`). Thin wrapper that
/// always runs in dry-run mode and translates `o->reset` into the FORCE flag.
///
/// `is_submodule` mirrors `submodule_from_ce(ce)`: when the cache entry is not a
/// submodule (no `.gitmodules` binding), the check is a no-op `Ok`. The caller
/// resolves that from the typed [`crate::config::SubmoduleConfigSet`].
pub fn check_submodule_move_head(
    is_submodule: bool,
    ctx: &MoveHeadContext,
    old_id: Option<&str>,
    new_id: Option<&str>,
    reset: bool,
) -> MoveHeadVerdict {
    if !is_submodule {
        return MoveHeadVerdict::Ok;
    }
    let flags = MoveHeadFlags { force: reset };
    check_move_head(ctx, old_id, new_id, flags)
}

/// Port of `verify_clean_submodule` (git `unpack-trees.c`). Checks that checking
/// out `ce`'s oid in the submodule subdir won't overwrite working files, given
/// the submodule's current head (`old_sha1`, `None` when HEAD is unresolved).
///
/// In git this is literally `check_submodule_move_head(ce, old_sha1,
/// oid_to_hex(&ce->oid), o)` with the same `submodule_from_ce` short-circuit.
pub fn verify_clean_submodule(
    is_submodule: bool,
    ctx: &MoveHeadContext,
    old_sha1: Option<&str>,
    new_oid_hex: &str,
    reset: bool,
) -> MoveHeadVerdict {
    if !is_submodule {
        return MoveHeadVerdict::Ok;
    }
    check_submodule_move_head(true, ctx, old_sha1, Some(new_oid_hex), reset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(active: bool, populated: bool, dirty: bool) -> MoveHeadContext {
        MoveHeadContext {
            active,
            populated,
            has_dirty_index: dirty,
        }
    }

    #[test]
    fn inactive_submodule_is_always_ok() {
        let c = ctx(false, true, true);
        assert_eq!(
            check_move_head(&c, Some("old"), Some("new"), MoveHeadFlags { force: false }),
            MoveHeadVerdict::Ok
        );
    }

    #[test]
    fn unpopulated_with_old_head_is_ok() {
        let c = ctx(true, false, true);
        assert_eq!(
            check_move_head(&c, Some("old"), Some("new"), MoveHeadFlags::default()),
            MoveHeadVerdict::Ok
        );
    }

    #[test]
    fn dirty_index_blocks_non_forced_move() {
        let c = ctx(true, true, true);
        assert_eq!(
            check_move_head(&c, Some("old"), Some("new"), MoveHeadFlags { force: false }),
            MoveHeadVerdict::WouldLose
        );
    }

    #[test]
    fn force_bypasses_dirty_index() {
        let c = ctx(true, true, true);
        assert_eq!(
            check_move_head(&c, Some("old"), Some("new"), MoveHeadFlags { force: true }),
            MoveHeadVerdict::Ok
        );
    }

    #[test]
    fn coming_into_existence_skips_dirty_check() {
        // old_head == None: submodule is new in the target tree, no dirty
        // gating applies.
        let c = ctx(true, true, true);
        assert_eq!(
            check_move_head(&c, None, Some("new"), MoveHeadFlags::default()),
            MoveHeadVerdict::Ok
        );
    }

    #[test]
    fn unpack_trees_wrappers_short_circuit_non_submodule() {
        let c = ctx(true, true, true);
        assert_eq!(
            check_submodule_move_head(false, &c, Some("old"), Some("new"), false),
            MoveHeadVerdict::Ok
        );
        assert_eq!(
            verify_clean_submodule(false, &c, Some("old"), "newoid", false),
            MoveHeadVerdict::Ok
        );
    }

    #[test]
    fn verify_clean_submodule_rejects_dirty() {
        let c = ctx(true, true, true);
        assert_eq!(
            verify_clean_submodule(true, &c, Some("old"), "newoid", false),
            MoveHeadVerdict::WouldLose
        );
    }

    #[test]
    fn reset_sets_force() {
        let c = ctx(true, true, true);
        // reset=true → force, so dirty index does NOT block.
        assert_eq!(
            check_submodule_move_head(true, &c, Some("old"), Some("new"), true),
            MoveHeadVerdict::Ok
        );
    }
}
