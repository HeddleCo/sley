//! Engine parity tests: library APIs vs oracle git.
//!
//! Ported from `crates/sley-cli/tests/{rev_parse,cat_file,config}.rs` patterns.

mod cat_file;
mod common;
mod config;
mod diff;
mod hash_object;
mod index;
mod init;
mod objects;
mod refs;
mod remote;
mod resolve_path;
mod rev_graph;
mod rev_parse;
mod update_index;
mod write_tree;
