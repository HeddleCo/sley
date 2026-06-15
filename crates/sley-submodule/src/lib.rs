//! `sley-submodule` — shared submodule engine for sley.
//!
//! Centralizes two pieces of submodule logic that git keeps in
//! `submodule-config.c` and `submodule.c`, and that sley previously scattered
//! across the 1850-line `submodule` CLI command and ~14 ad-hoc `.gitmodules`
//! walks:
//!
//! - [`config`] — typed `.gitmodules` parsing (`submodule-config.c`): a
//!   [`config::SubmoduleConfigSet`] of typed [`config::Submodule`] entries, plus
//!   the `check_submodule_name` / `check_submodule_url` security checks and the
//!   recurse-mode / update-strategy enums.
//! - [`move_head`] — the move-head / verify-clean primitives
//!   (`submodule.c::submodule_move_head` dry-run path + the `unpack-trees.c`
//!   wrappers `check_submodule_move_head` / `verify_clean_submodule`). These are
//!   the hooks the tree-switch (unpack-trees) engine calls to decide whether a
//!   HEAD move would lose submodule work.
//!
//! The two halves are paired on purpose: a tree-switch needs the typed config
//! (to know *which* paths are submodules and their bindings) AND the move-head
//! check (to know whether moving each one is safe).
//!
//! - [`apply`] — the gitlink *worktree-apply* primitive (`entry.c`'s
//!   `S_IFGITLINK` create + `remove_or_warn`/`rmdir_or_warn` drop branches):
//!   one decision table over appearing / disappearing gitlinks that every
//!   tree-applying command (checkout / reset / read-tree / merge) routes
//!   through, so an appearing submodule always leaves an empty placeholder
//!   directory and a disappearing one is always left in place.

pub mod apply;
pub mod config;
pub mod move_head;

pub use apply::{
    Directive, GITLINK_MODE, OnDisk, apply_appearing_gitlink, apply_disappearing_gitlink,
    gitlink_apply_appearing, gitlink_apply_disappearing, is_gitlink, probe_on_disk, worktree_join,
};
pub use config::{
    ParseWarning, RecurseMode, Submodule, SubmoduleConfigSet, UpdateStrategy, UpdateType,
    check_submodule_name, check_submodule_url, looks_like_command_line_option, parse_fetch_recurse,
    parse_update_strategy, parse_update_type, update_type_to_string,
};
pub use move_head::{
    MoveHeadContext, MoveHeadFlags, MoveHeadVerdict, check_move_head, check_submodule_move_head,
    verify_clean_submodule,
};
