//! CLI command implementations, extracted from the crate root in verified waves.
//!
//! Each submodule owns a cohesive group of commands. Shared helpers remain at the
//! crate root and are reachable here because a submodule can access its ancestor
//! modules' private items; the only items a submodule must expose are the
//! `cmd_*` entry points the dispatcher in `run` calls, which are `pub(crate)`.

pub(crate) mod trees;
