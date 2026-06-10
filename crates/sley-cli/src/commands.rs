//! CLI command implementations, extracted from the crate root in verified waves.
//!
//! Each submodule owns a cohesive group of commands. Shared helpers remain at the
//! crate root and are reachable here because a submodule can access its ancestor
//! modules' private items; the only items a submodule must expose are the
//! `cmd_*` entry points the dispatcher in `run` calls, which are `pub(crate)`.

pub(crate) mod alias;
pub(crate) mod am;
pub(crate) mod args;
pub(crate) mod attrs;
pub(crate) mod bisect;
pub(crate) mod blame;
pub(crate) mod branch;
pub(crate) mod cat_file;
pub(crate) mod checkout_index;
pub(crate) mod config_cmd;
pub(crate) mod describe;
pub(crate) mod diff;
pub(crate) mod diff_files;
pub(crate) mod diff_index;
pub(crate) mod diff_tree;
pub(crate) mod format_patch;
pub(crate) mod grep;
pub(crate) mod hash_object;
pub(crate) mod hash_object_fsck;
pub(crate) mod index;
pub(crate) mod interpret_trailers;
pub(crate) mod log;
pub(crate) mod merge_file;
pub(crate) mod merge_rebase;
pub(crate) mod merge_tree;
pub(crate) mod mktag;
pub(crate) mod name_rev;
pub(crate) mod notes;
pub(crate) mod pack;
pub(crate) mod pack_objects;
pub(crate) mod patch_id;
pub(crate) mod read_tree;
pub(crate) mod refs;
pub(crate) mod remote_cmds;
pub(crate) mod rev_list;
pub(crate) mod rev_parse;
pub(crate) mod shortlog;
pub(crate) mod show;
pub(crate) mod show_branch;
pub(crate) mod sparse_checkout;
pub(crate) mod stash;
pub(crate) mod tag;
pub(crate) mod trees;
pub(crate) mod verify_commit;
pub(crate) mod verify_tag;
pub(crate) mod worktree;
