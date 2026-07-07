// sley#7: untrusted-input parsing crate — fallible ops propagate errors;
// the only retained `expect`s would be documented compile-time invariants.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod pktline;
mod sideband;
mod v0;
mod v1;
mod v2;
mod upload_pack;
mod receive_pack;

#[cfg(test)]
mod tests;

pub use pktline::*;
pub use sideband::*;
pub use v0::*;

pub use v2::*;
pub use upload_pack::*;
pub use receive_pack::*;
