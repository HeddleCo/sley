//! Repo-independent mail/text engines for the sley project.
//!
//! Everything in this crate is pure byte/text processing with no repository,
//! worktree, refs, or config dependencies: the only in-workspace dependency is
//! `sley-core` (object-format constants + hashing, date helpers, `GitError`).
//! The CLI (`sley-cli`) keeps thin delegations so all call sites compile
//! unchanged.
//!
//! * [`mailinfo`] — mbox/mboxrd splitting, RFC 822 header parsing, RFC 2047
//!   encoded-word decoding, RFC 2822/asctime date parsing, subject cleanup,
//!   and stgit/hg patch-to-mail conversion (the `git mailinfo`/`git mailsplit`
//!   engines behind `git am`).
//! * [`encode`] — format-patch mail encoders: RFC 2047 encoded-word writing,
//!   RFC 822 address quoting, header word-wrapping, MIME multipart framing,
//!   Message-ID / In-Reply-To / References threading, and subject-paragraph
//!   extraction.
//! * [`patch_id`] — the `git patch-id` hash core (stable/unstable digest
//!   folding over pre-split diff lines).
//! * [`trailers`] — the `git interpret-trailers` engine (a port of git's
//!   `trailer.c`): trailer-block detection, parsing, application policies,
//!   and rendering. Configuration loading from `GitConfig` stays in the CLI;
//!   this module works on plain-data types only.

pub mod encode;
pub mod mailinfo;
pub mod patch_id;
pub mod trailers;
