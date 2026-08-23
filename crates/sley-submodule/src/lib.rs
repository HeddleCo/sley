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
//! - [`relative_url()`] — the `.gitmodules`-url → concrete-url resolution
//!   (`remote.c::relative_url` + `submodule--helper.c::resolve_relative_url`).
//!   `submodule init` / `sync` / `add` all route their relative-url math through
//!   this one primitive so every git URL form (ssh, scp-style, file://, helper,
//!   relative local path) resolves byte-for-byte the way upstream does.
//! - [`update_strategy`] — the `submodule update` mode resolver
//!   (`submodule--helper.c::determine_submodule_update_strategy`): picks ONE of
//!   checkout/merge/rebase/command/none from the CLI-flag → `.git/config` →
//!   `.gitmodules` → default-checkout precedence, with the `just_cloned`
//!   downgrade. `submodule update` routes every mode through this one resolver.
//! - [`move_head`] — the move-head / verify-clean primitives
//!   (`submodule.c::submodule_move_head` dry-run path + the `unpack-trees.c`
//!   wrappers `check_submodule_move_head` / `verify_clean_submodule`). These are
//!   the hooks the tree-switch (unpack-trees) engine calls to decide whether a
//!   HEAD move would lose submodule work.
//!
//! The two halves are paired on purpose: a tree-switch needs the typed config
//! (to know *which* paths are submodules and their bindings) AND the move-head
//! check (to know whether moving each one is safe).

pub mod config;
pub mod history;
pub mod move_head;
pub mod operations;
pub mod path_safety;
pub mod relative_url;
pub mod update_strategy;

pub use config::{
    ParseWarning, RecurseMode, Submodule, SubmoduleConfigSet, UpdateStrategy, UpdateType,
    check_submodule_name, check_submodule_url, looks_like_command_line_option, parse_fetch_recurse,
    parse_update_strategy, parse_update_type, update_type_to_string,
};
pub use move_head::{
    MoveHeadContext, MoveHeadFlags, MoveHeadVerdict, check_move_head, check_submodule_move_head,
    verify_clean_submodule,
};
pub use operations::{
    ConfigMutation, InitCandidate, InitPlan, InitPlanError, InitRegistration, apply_init_plan,
    plan_init,
};
pub use path_safety::submodule_path_has_symlink_parent;
pub use relative_url::{relative_url, resolve_relative_url};
pub use update_strategy::determine_update_strategy;
