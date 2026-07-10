//! Native entry point for Git's `remote-http` helper protocol.

use sley::{GitError, Result};

/// Enter the smart-HTTP remote-helper surface.
///
/// Clone, fetch, and push already use Sley's native HTTP transport directly.
/// The dashed helper entry point is retained for byte-compatible helper
/// discovery and diagnostics; its no-argument contract is observable outside
/// a repository and must identify itself as `remote-curl`, as upstream does.
pub(crate) fn cmd_remote_http(args: &[String]) -> Result<()> {
    if args.is_empty() {
        eprintln!("error: remote-curl: usage: git remote-curl <remote> [<url>]");
        return Err(GitError::Exit(1));
    }

    eprintln!("error: remote-curl: the remote-helper command loop is not yet implemented");
    Err(GitError::Exit(1))
}
