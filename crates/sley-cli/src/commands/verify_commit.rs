//! `git verify-commit` — check the GPG signature of commit objects.
//!
//! Real `git verify-commit <commit>...` resolves each argument to an object,
//! confirms it is a commit, locates the embedded `gpgsig`/`gpgsig-sha256` header,
//! and hands the signed payload to `gpg --verify`. This reimplementation does not
//! carry a GPG backend, so it reproduces every part of the contract that does not
//! depend on a signature actually verifying:
//!
//!   * An *unsigned* commit cannot be verified, so git exits non-zero and prints
//!     nothing at all (even under `-v`). We mirror that byte-for-byte: silent
//!     stderr/stdout, exit 1.
//!   * A non-commit object (tree, blob, or an annotated tag — `verify-commit`
//!     does not peel tags) reports
//!     `error: <arg>: cannot verify a non-commit object of type <type>.` where
//!     `<arg>` is the user's argument verbatim, exit 1.
//!   * An unresolvable argument reports `error: commit '<arg>' not found.`,
//!     exit 1.
//!   * All arguments are processed; the command exits 1 if *any* failed.
//!   * `-h`/`--help` print usage to stdout; a bad option prints
//!     `error: unknown option/switch ...` then usage to stderr; both exit 129.
//!   * `-v`/`--verbose` asks git to print the commit payload (signature stripped)
//!     before the gpg status. Since only *signed* commits ever produce that
//!     payload and we cannot verify signatures, a signed commit is reported as an
//!     unsupported operation after optionally echoing its payload.
//!
//! See `commands::tag::verify_tag` for the sibling tag-side logic; this module
//! follows the same glob-import + private-helper structure as the other
//! self-contained command modules (`commands::branch`, `commands::stash`).

// Glob the crate root for shared plumbing (RepositoryContext, the ObjectReader
// trait, ObjectType, Commit, GitError, io, etc.); see commands::stash for the
// rationale behind the wildcard import.
use crate::*;

/// Entry point for `git verify-commit`.
pub(crate) fn cmd_verify_commit(args: &[String]) -> Result<()> {
    let options = match parse_verify_commit_args(args)? {
        VerifyCommitInvocation::Run(options) => options,
        VerifyCommitInvocation::Help => {
            print!("{VERIFY_COMMIT_USAGE}");
            io::stdout().flush()?;
            return Err(GitError::Exit(129));
        }
    };

    if options.commits.is_empty() {
        // `git verify-commit` with no commit-ish is a usage error (exit 129),
        // distinct from the verification-failure exit code (1).
        eprint!("{VERIFY_COMMIT_USAGE}");
        return Err(GitError::Exit(129));
    }

    let repo = RepositoryContext::discover_current()?;

    // git verifies every argument and only then reports overall failure, so a bad
    // early argument never short-circuits a later one. Accumulate failures and map
    // them to a single exit-1 at the end.
    let mut failed = false;
    for commit in &options.commits {
        if !verify_one_commit(&repo, commit, &options)? {
            failed = true;
        }
    }
    if failed {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

/// Flags accepted by `git verify-commit` (besides the commit-ish operands).
#[derive(Debug)]
struct VerifyCommitOptions {
    /// `-v`/`--verbose`: print the commit payload (signature stripped) before the
    /// gpg status output.
    verbose: bool,
    /// `--raw`: emit gpg's raw status lines instead of the human-readable summary.
    /// Accepted for compatibility; it only affects signed commits, which this
    /// implementation cannot verify.
    raw: bool,
    /// The commit-ish operands, in the order given on the command line.
    commits: Vec<String>,
}

/// The outcome of argument parsing: either a runnable invocation or a request for
/// the help text (`-h`/`--help`).
#[derive(Debug)]
enum VerifyCommitInvocation {
    Run(VerifyCommitOptions),
    Help,
}

/// Parse `verify-commit` arguments, matching git's option grammar and its
/// exit-129 errors for unknown options/switches.
fn parse_verify_commit_args(args: &[String]) -> Result<VerifyCommitInvocation> {
    let mut verbose = false;
    let mut raw = false;
    let mut commits = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                // Everything after `--` is a commit-ish, even if it looks like a flag.
                commits.extend(iter.cloned());
                break;
            }
            "-h" | "--help" => return Ok(VerifyCommitInvocation::Help),
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "--raw" => raw = true,
            "--no-raw" => raw = false,
            value if value.starts_with("--verbose=") => {
                return verify_commit_option_takes_no_value_error("verbose");
            }
            value if value.starts_with("--raw=") => {
                return verify_commit_option_takes_no_value_error("raw");
            }
            value if value.starts_with("--") => {
                return verify_commit_unknown_option_error(value.trim_start_matches("--"));
            }
            value if value.starts_with('-') && value.len() > 1 => {
                // Reject the first unrecognized short switch, mirroring git's
                // single-character `error: unknown switch` message. Known short
                // switches bundled together (e.g. `-vv`) are all `-v`.
                if let Some(switch) = verify_commit_unknown_short_switch(value) {
                    return verify_commit_unknown_switch_error(switch);
                }
                verbose = true;
            }
            value => commits.push(value.to_string()),
        }
    }
    Ok(VerifyCommitInvocation::Run(VerifyCommitOptions {
        verbose,
        raw,
        commits,
    }))
}

/// Verify a single commit-ish. Returns `Ok(true)` when verification "succeeds"
/// from the caller's perspective (which, lacking a GPG backend, never happens for
/// real signatures) and `Ok(false)` for every git-reported failure so the caller
/// can aggregate the exit code.
fn verify_one_commit(
    repo: &RepositoryContext,
    commit: &str,
    options: &VerifyCommitOptions,
) -> Result<bool> {
    // git resolves the argument without peeling annotated tags: an annotated tag
    // surfaces as a non-commit object below, matching real `verify-commit`.
    let oid = match repo.resolve_revision(commit) {
        Ok(oid) => oid,
        Err(
            GitError::NotFound(_)
            | GitError::InvalidFormat(_)
            | GitError::InvalidPath(_)
            | GitError::InvalidObjectId(_)
            | GitError::InvalidObject(_)
            | GitError::Unsupported(_),
        ) => {
            eprintln!("error: commit '{commit}' not found.");
            return Ok(false);
        }
        Err(err) => return Err(err),
    };

    // A full-length hex argument resolves to an object id without an existence
    // check, so the read can still miss. git distinguishes this from an
    // unresolvable name: a parseable-but-absent object reports "unable to read
    // file" (echoing the argument as given), versus "not found" above. Any other
    // read error (corruption) is reported the same way, matching git's
    // parse-object failure path. Processing continues so every operand is tried.
    let object = match repo.objects().read_object(&oid) {
        Ok(object) => object,
        Err(_) => {
            eprintln!("error: {commit}: unable to read file.");
            return Ok(false);
        }
    };

    if object.object_type != ObjectType::Commit {
        eprintln!(
            "error: {commit}: cannot verify a non-commit object of type {}.",
            object.object_type.as_str()
        );
        return Ok(false);
    }

    let Some((payload, signature)) = commands::signing::commit_signature_payload(&object.body)
    else {
        // An unsigned commit cannot be verified: git prints nothing — not even
        // under `-v` — and exits non-zero. Reproduce that silence exactly.
        return Ok(false);
    };

    if options.verbose {
        io::stdout().write_all(&payload)?;
        io::stdout().flush()?;
    }
    let verification =
        commands::signing::verify_payload(repo.git_dir(), Some(repo.config()), &payload, &signature)?;
    if options.raw {
        io::stderr().write_all(&verification.status_output)?;
    } else {
        io::stderr().write_all(&verification.human_output)?;
    }
    Ok(verification.success)
}

/// The signature header keys git recognizes on a commit object.
const COMMIT_SIGNATURE_HEADERS: [&[u8]; 2] = [b"gpgsig", b"gpgsig-sha256"];

/// Return true when the commit object body carries a signature header. Only the
/// header block (everything before the first blank line) is examined, and only at
/// the start of a (possibly continued) header line, matching git's parser.
fn commit_object_is_signed(body: &[u8]) -> bool {
    for line in commit_header_lines(body) {
        for key in COMMIT_SIGNATURE_HEADERS {
            if header_line_has_key(line, key) {
                return true;
            }
        }
    }
    false
}

/// Reconstruct the commit payload with its signature header removed, the way git
/// presents a signed commit under `-v`. Folded continuation lines (those starting
/// with a space) belonging to the signature header are dropped along with it.
fn commit_payload_without_signature(body: &[u8]) -> Vec<u8> {
    let Some(header_end) = header_block_end(body) else {
        // No header/message separator: nothing we can safely strip.
        return body.to_vec();
    };
    let header = &body[..header_end];
    let rest = &body[header_end..];

    let mut out = Vec::with_capacity(body.len());
    let mut skipping = false;
    for line in header.split_inclusive(|byte| *byte == b'\n') {
        let content = strip_trailing_newline(line);
        let is_continuation = content.first() == Some(&b' ');
        if skipping && is_continuation {
            // Folded continuation of the signature header; drop it.
            continue;
        }
        skipping = false;
        if COMMIT_SIGNATURE_HEADERS
            .iter()
            .any(|key| header_line_has_key(content, key))
        {
            skipping = true;
            continue;
        }
        out.extend_from_slice(line);
    }
    out.extend_from_slice(rest);
    out
}

/// Iterate the raw header lines (without trailing newlines) of a commit object,
/// stopping at the blank line that separates headers from the message.
fn commit_header_lines(body: &[u8]) -> impl Iterator<Item = &[u8]> {
    let end = header_block_end(body).unwrap_or(body.len());
    body[..end]
        .split(|byte| *byte == b'\n')
        .take_while(|line| !line.is_empty())
}

/// Byte offset just past the header/message separator (`"\n\n"`), or `None` when
/// the object has no separator (a malformed commit).
fn header_block_end(body: &[u8]) -> Option<usize> {
    body.windows(2)
        .position(|window| window == b"\n\n")
        .map(|idx| idx + 1)
}

/// True when `line` is a header line whose key is exactly `key` (i.e. the key is
/// followed by a space, as in `"gpgsig -----BEGIN..."`).
fn header_line_has_key(line: &[u8], key: &[u8]) -> bool {
    line.len() > key.len() && line.starts_with(key) && line[key.len()] == b' '
}

fn strip_trailing_newline(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

/// Return the first short switch in a bundled `-xyz` argument that
/// `verify-commit` does not recognize, or `None` if every switch is known.
fn verify_commit_unknown_short_switch(value: &str) -> Option<char> {
    value[1..].chars().find(|switch| !matches!(switch, 'v'))
}

fn verify_commit_unknown_option_error(option: &str) -> Result<VerifyCommitInvocation> {
    eprintln!("error: unknown option `{option}'");
    eprint!("{VERIFY_COMMIT_USAGE}");
    Err(GitError::Exit(129))
}

fn verify_commit_unknown_switch_error(switch: char) -> Result<VerifyCommitInvocation> {
    eprintln!("error: unknown switch `{switch}'");
    eprint!("{VERIFY_COMMIT_USAGE}");
    Err(GitError::Exit(129))
}

fn verify_commit_option_takes_no_value_error(option: &str) -> Result<VerifyCommitInvocation> {
    // git's parse-options prints only the error for a "takes no value" rejection,
    // without the usage block (unlike the unknown-option/switch errors above).
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

/// The exact usage block git prints for `verify-commit` (stdout for `-h`, stderr
/// for errors). Reproduced byte-for-byte, including the trailing blank line.
const VERIFY_COMMIT_USAGE: &str = "\
usage: git verify-commit [-v | --verbose] [--raw] <commit>...

    -v, --[no-]verbose    print commit contents
    --[no-]raw            print raw gpg status output

";

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_commit_body() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"tree 0d8a474fc67971fb3dd7616e26323d3066442555\n");
        body.extend_from_slice(b"author Tester <tester@example.com> 1790000000 -0500\n");
        body.extend_from_slice(b"committer Tester <tester@example.com> 1790000000 -0500\n");
        body.extend_from_slice(b"gpgsig -----BEGIN PGP SIGNATURE-----\n");
        body.extend_from_slice(b" \n");
        body.extend_from_slice(b" iQEzBAABCAAdSomeBase64Here\n");
        body.extend_from_slice(b" -----END PGP SIGNATURE-----\n");
        body.extend_from_slice(b"\n");
        body.extend_from_slice(b"signed commit\n");
        body
    }

    fn unsigned_commit_body() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"tree 0d8a474fc67971fb3dd7616e26323d3066442555\n");
        body.extend_from_slice(b"author Tester <tester@example.com> 1790000000 -0500\n");
        body.extend_from_slice(b"committer Tester <tester@example.com> 1790000000 -0500\n");
        body.extend_from_slice(b"\n");
        body.extend_from_slice(b"unsigned commit\n");
        body
    }

    #[test]
    fn detects_gpgsig_header() {
        assert!(commit_object_is_signed(&signed_commit_body()));
    }

    #[test]
    fn detects_sha256_signature_header() {
        let mut body = Vec::new();
        body.extend_from_slice(b"tree 0d8a474fc67971fb3dd7616e26323d3066442555\n");
        body.extend_from_slice(b"committer Tester <tester@example.com> 1790000000 -0500\n");
        body.extend_from_slice(b"gpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n");
        body.extend_from_slice(b" base64\n");
        body.extend_from_slice(b" -----END PGP SIGNATURE-----\n");
        body.extend_from_slice(b"\nmsg\n");
        assert!(commit_object_is_signed(&body));
    }

    #[test]
    fn unsigned_commit_is_not_signed() {
        assert!(!commit_object_is_signed(&unsigned_commit_body()));
    }

    #[test]
    fn signature_in_message_body_is_ignored() {
        // A "-----BEGIN PGP SIGNATURE-----" line that lives in the commit message
        // (not the header block) must not be treated as a signature header.
        let mut body = Vec::new();
        body.extend_from_slice(b"tree 0d8a474fc67971fb3dd7616e26323d3066442555\n");
        body.extend_from_slice(b"committer Tester <tester@example.com> 1790000000 -0500\n");
        body.extend_from_slice(b"\n");
        body.extend_from_slice(b"gpgsig is mentioned here but not a header\n");
        assert!(!commit_object_is_signed(&body));
    }

    #[test]
    fn payload_strips_signature_header_block() {
        let stripped = commit_payload_without_signature(&signed_commit_body());
        let text = String::from_utf8(stripped).expect("utf8");
        assert!(!text.contains("gpgsig"));
        assert!(!text.contains("BEGIN PGP SIGNATURE"));
        assert!(text.contains("tree 0d8a474fc67971fb3dd7616e26323d3066442555\n"));
        assert!(text.contains("committer Tester"));
        assert!(text.ends_with("\nsigned commit\n"));
        // The author header before the signature survives.
        assert!(text.contains("author Tester <tester@example.com> 1790000000 -0500\n"));
    }

    #[test]
    fn payload_unchanged_for_unsigned_commit() {
        let body = unsigned_commit_body();
        assert_eq!(commit_payload_without_signature(&body), body);
    }

    #[test]
    fn parses_verbose_and_raw_flags() {
        let args = vec![
            "-v".to_string(),
            "--raw".to_string(),
            "deadbeef".to_string(),
        ];
        match parse_verify_commit_args(&args).expect("parse") {
            VerifyCommitInvocation::Run(options) => {
                assert!(options.verbose);
                assert!(options.raw);
                assert_eq!(options.commits, vec!["deadbeef".to_string()]);
            }
            VerifyCommitInvocation::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn double_dash_terminates_options() {
        let args = vec!["--".to_string(), "-v".to_string()];
        match parse_verify_commit_args(&args).expect("parse") {
            VerifyCommitInvocation::Run(options) => {
                assert!(!options.verbose);
                assert_eq!(options.commits, vec!["-v".to_string()]);
            }
            VerifyCommitInvocation::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn help_flag_requests_help() {
        let args = vec!["-h".to_string()];
        assert!(matches!(
            parse_verify_commit_args(&args).expect("parse"),
            VerifyCommitInvocation::Help
        ));
    }

    #[test]
    fn unknown_long_option_is_exit_129() {
        let args = vec!["--bogus".to_string()];
        match parse_verify_commit_args(&args) {
            Err(GitError::Exit(129)) => {}
            other => panic!("expected exit 129, got {other:?}"),
        }
    }

    #[test]
    fn unknown_short_switch_is_exit_129() {
        let args = vec!["-z".to_string()];
        match parse_verify_commit_args(&args) {
            Err(GitError::Exit(129)) => {}
            other => panic!("expected exit 129, got {other:?}"),
        }
    }
}
