//! `git verify-tag` — check the GPG signature of annotated tag objects.
//!
//! Real `git verify-tag <tag>...` resolves each argument to an object **without
//! peeling** (a lightweight tag therefore surfaces as the commit it points at),
//! confirms the object is an annotated tag, splits the tag body at its trailing
//! PGP signature, and hands the signed payload to `gpg`/`gpgsm`/`ssh-keygen`.
//! This reimplementation carries no signature-verification backend, so it
//! reproduces every part of the contract that does not depend on a signature
//! actually verifying:
//!
//!   * An *unsigned* annotated tag has no signature to check: git prints
//!     `error: no signature found` to stderr and exits 1. Unlike
//!     `verify-commit`, `verify-tag` is **not** silent here — and under `-v` it
//!     first writes the tag's full raw body to stdout. We mirror both exactly.
//!   * A non-tag object (commit — including a lightweight tag's target — tree, or
//!     blob) reports
//!     `error: <arg>: cannot verify a non-tag object of type <type>.` where
//!     `<arg>` is the user's argument verbatim, exit 1.
//!   * An unresolvable argument reports `error: tag '<arg>' not found.`, exit 1.
//!   * All arguments are processed; the command exits 1 if *any* failed.
//!   * `-h`/`--help` print usage to stdout (exit 129); an unknown option/switch
//!     prints `error: unknown option/switch ...` followed by the usage block to
//!     stderr (exit 129); an option that mishandles its value (`--verbose=x`,
//!     `--raw=x`, a bare `--format`) prints only the one-line parse-options error
//!     — *without* the usage block — and exits 129.
//!   * `-v`/`--verbose` asks git to print the tag payload before the gpg status.
//!     For a *signed* tag the payload has its signature stripped; for an unsigned
//!     tag the entire body is printed. We reproduce the payload echo, but cannot
//!     perform signature verification, so a signed tag is reported as an
//!     unsupported operation after optionally echoing its payload.
//!
//! See `commands::verify_commit` for the sibling commit-side logic; this module
//! follows the same glob-import + private-helper structure as the other
//! self-contained command modules (`commands::branch`, `commands::stash`).

// Glob the crate root for shared plumbing (RepositoryContext, the ObjectReader
// trait, ObjectType, GitError, io, etc.); see commands::stash for the rationale
// behind the wildcard import.
use crate::*;

/// Entry point for `git verify-tag`.
pub(crate) fn cmd_verify_tag(args: &[String]) -> Result<()> {
    let options = match parse_verify_tag_args(args)? {
        VerifyTagInvocation::Run(options) => options,
        VerifyTagInvocation::Help => {
            print!("{VERIFY_TAG_USAGE}");
            io::stdout().flush()?;
            return Err(GitError::Exit(129));
        }
    };

    if options.tags.is_empty() {
        // `git verify-tag` with no tag operand is a usage error (exit 129),
        // distinct from the verification-failure exit code (1).
        eprint!("{VERIFY_TAG_USAGE}");
        return Err(GitError::Exit(129));
    }

    let repo = RepositoryContext::discover_current()?;

    // git verifies every argument and only then reports overall failure, so a bad
    // early argument never short-circuits a later one. Accumulate failures and map
    // them to a single exit-1 at the end.
    let mut failed = false;
    for tag in &options.tags {
        if !verify_one_tag(&repo, tag, &options)? {
            failed = true;
        }
    }
    if failed {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

/// Flags accepted by `git verify-tag` (besides the tag operands).
#[derive(Debug)]
struct VerifyTagOptions {
    /// `-v`/`--verbose`: print the tag payload before the gpg status output.
    verbose: bool,
    /// `--raw`: emit gpg's raw status lines instead of the human-readable summary.
    /// Accepted for compatibility; it only affects signed tags, which this
    /// implementation cannot verify.
    raw: bool,
    /// `--format=<format>`: a `cat-file`-style format for the per-tag output.
    /// Accepted for compatibility; git only emits it on a *successful*
    /// verification, which cannot happen without a signature backend.
    format: Option<String>,
    /// The tag operands, in the order given on the command line.
    tags: Vec<String>,
}

/// The outcome of argument parsing: either a runnable invocation or a request for
/// the help text (`-h`/`--help`).
#[derive(Debug)]
enum VerifyTagInvocation {
    Run(VerifyTagOptions),
    Help,
}

/// Parse `verify-tag` arguments, matching git's option grammar and its exit-129
/// errors for unknown options/switches and for value mishandling.
fn parse_verify_tag_args(args: &[String]) -> Result<VerifyTagInvocation> {
    let mut verbose = false;
    let mut raw = false;
    let mut format: Option<String> = None;
    let mut tags = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                // Everything after `--` is a tag operand, even if it looks like a flag.
                tags.extend(iter.cloned());
                break;
            }
            "-h" | "--help" => return Ok(VerifyTagInvocation::Help),
            "-v" | "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "--raw" => raw = true,
            "--no-raw" => raw = false,
            // `--format` consumes the following argument as its value; a missing
            // value is git's `requires a value` parse-options error.
            "--format" => match iter.next() {
                Some(value) => format = Some(value.clone()),
                None => return verify_tag_option_requires_value_error("format"),
            },
            // `--no-format` clears any format back to the default (git treats it as
            // an empty format string, which we model as "no explicit format").
            "--no-format" => format = None,
            value if value.starts_with("--format=") => {
                format = Some(value["--format=".len()..].to_string());
            }
            value if value.starts_with("--verbose=") => {
                return verify_tag_option_takes_no_value_error("verbose");
            }
            value if value.starts_with("--raw=") => {
                return verify_tag_option_takes_no_value_error("raw");
            }
            value if value.starts_with("--") => {
                return verify_tag_unknown_option_error(value.trim_start_matches("--"));
            }
            value if value.starts_with('-') && value.len() > 1 => {
                // Reject the first unrecognized short switch, mirroring git's
                // single-character `error: unknown switch` message. Known short
                // switches bundled together (e.g. `-vv`) are all `-v`.
                if let Some(switch) = verify_tag_unknown_short_switch(value) {
                    return verify_tag_unknown_switch_error(switch);
                }
                verbose = true;
            }
            value => tags.push(value.to_string()),
        }
    }
    Ok(VerifyTagInvocation::Run(VerifyTagOptions {
        verbose,
        raw,
        format,
        tags,
    }))
}

/// Verify a single tag operand. Returns `Ok(true)` when verification "succeeds"
/// from the caller's perspective (which, lacking a signature backend, never
/// happens for real signatures) and `Ok(false)` for every git-reported failure so
/// the caller can aggregate the exit code.
fn verify_one_tag(repo: &RepositoryContext, tag: &str, options: &VerifyTagOptions) -> Result<bool> {
    // git resolves the argument *without* peeling: a lightweight tag (a ref that
    // points straight at a commit) surfaces as that commit below and is reported
    // as a non-tag object, matching real `verify-tag`.
    let oid = match repo.resolve_revision(tag) {
        Ok(oid) => oid,
        Err(
            GitError::NotFound(_)
            | GitError::InvalidFormat(_)
            | GitError::InvalidPath(_)
            | GitError::InvalidObjectId(_)
            | GitError::InvalidObject(_)
            | GitError::Unsupported(_),
        ) => {
            eprintln!("error: tag '{tag}' not found.");
            return Ok(false);
        }
        Err(err) => return Err(err),
    };

    // `resolve_revision` returns a valid object id for any full-length hex string
    // *without* checking that the object exists (matching git's `get_oid`, which
    // parses a complete oid directly). So a well-formed-but-absent oid — e.g. the
    // all-zeros id — resolves here and then fails to read. git, in that case, asks
    // the object database for the type, gets "none", and reports it as a non-tag
    // object of type `(null)` rather than "tag not found" (the latter is reserved
    // for arguments that never resolve to an oid at all, handled above).
    let object = match repo.objects().read_object(&oid) {
        Ok(object) => object,
        Err(_) => {
            eprintln!("error: {tag}: cannot verify a non-tag object of type (null).");
            return Ok(false);
        }
    };

    if object.object_type != ObjectType::Tag {
        eprintln!(
            "error: {tag}: cannot verify a non-tag object of type {}.",
            object.object_type.as_str()
        );
        return Ok(false);
    }

    // Locate the trailing PGP signature, if any, the way git's signature parser
    // does: an armor `-----BEGIN ... -----` marker at the start of a line.
    match tag_signature_offset(&object.body) {
        None => {
            // An unsigned annotated tag: git echoes the entire raw body under -v,
            // then reports the missing signature on stderr and exits non-zero.
            if options.verbose {
                io::stdout().write_all(&object.body)?;
                io::stdout().flush()?;
            }
            eprintln!("error: no signature found");
            Ok(false)
        }
        Some(signature_start) => {
            // A signed tag: git would echo the signature-stripped payload under -v
            // before running the signature backend. We can reproduce the payload
            // echo, but cannot verify signatures, so report the unsupported
            // operation afterwards.
            if options.verbose {
                io::stdout().write_all(&object.body[..signature_start])?;
                io::stdout().flush()?;
            }
            let _ = (&options.raw, &options.format);
            Err(GitError::Command(
                "signed tag verification is not implemented".into(),
            ))
        }
    }
}

/// The armor markers git's signature parser recognizes at the start of a line as
/// the beginning of a tag signature (see git's `gpg-interface.c`). A
/// `-----BEGIN GPG SIGNATURE-----` line is deliberately *not* in this set, matching
/// git, which reports such a tag as having no signature.
const TAG_SIGNATURE_MARKERS: [&[u8]; 4] = [
    b"-----BEGIN PGP SIGNATURE-----",
    b"-----BEGIN PGP MESSAGE-----",
    b"-----BEGIN SIGNED MESSAGE-----",
    b"-----BEGIN SSH SIGNATURE-----",
];

/// Return the byte offset at which the tag's trailing signature begins (the first
/// byte of the line carrying a recognized armor marker), or `None` when the tag
/// carries no signature. Matching git, a marker is only honored at the start of a
/// line, so a marker embedded inside the tag message is ignored.
fn tag_signature_offset(body: &[u8]) -> Option<usize> {
    let mut line_start = 0;
    while line_start < body.len() {
        let line_end = body[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(body.len(), |idx| line_start + idx);
        let line = &body[line_start..line_end];
        if TAG_SIGNATURE_MARKERS.contains(&line) {
            return Some(line_start);
        }
        // Advance past the newline (or to the end if this was the final line).
        line_start = if line_end < body.len() {
            line_end + 1
        } else {
            body.len()
        };
    }
    None
}

/// Return the first short switch in a bundled `-xyz` argument that `verify-tag`
/// does not recognize, or `None` if every switch is known.
fn verify_tag_unknown_short_switch(value: &str) -> Option<char> {
    value[1..].chars().find(|switch| !matches!(switch, 'v'))
}

fn verify_tag_unknown_option_error(option: &str) -> Result<VerifyTagInvocation> {
    eprintln!("error: unknown option `{option}'");
    eprint!("{VERIFY_TAG_USAGE}");
    Err(GitError::Exit(129))
}

fn verify_tag_unknown_switch_error(switch: char) -> Result<VerifyTagInvocation> {
    eprintln!("error: unknown switch `{switch}'");
    eprint!("{VERIFY_TAG_USAGE}");
    Err(GitError::Exit(129))
}

/// git's parse-options prints only this one line (no usage block) when an option
/// that takes no value is given one, e.g. `--verbose=1`.
fn verify_tag_option_takes_no_value_error(option: &str) -> Result<VerifyTagInvocation> {
    eprintln!("error: option `{option}' takes no value");
    Err(GitError::Exit(129))
}

/// git's parse-options prints only this one line (no usage block) when an option
/// that requires a value is given none, e.g. a trailing `--format`.
fn verify_tag_option_requires_value_error(option: &str) -> Result<VerifyTagInvocation> {
    eprintln!("error: option `{option}' requires a value");
    Err(GitError::Exit(129))
}

/// The exact usage block git prints for `verify-tag` (stdout for `-h`, stderr for
/// unknown-option errors). Reproduced byte-for-byte, including the trailing blank
/// line; the description column is aligned at 26 columns as git lays it out.
const VERIFY_TAG_USAGE: &str = "\
usage: git verify-tag [-v | --verbose] [--format=<format>] [--raw] <tag>...

    -v, --[no-]verbose    print tag contents
    --[no-]raw            print raw gpg status output
    --[no-]format <format>
                          format to use for the output

";

#[cfg(test)]
mod tests {
    use super::*;

    fn unsigned_tag_body() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"object 3b7a93fed59b0ad9f415fcfd96addb31ac0fb752\n");
        body.extend_from_slice(b"type commit\n");
        body.extend_from_slice(b"tag v1.0\n");
        body.extend_from_slice(b"tagger Tester <tester@example.com> 1790000000 -0500\n");
        body.extend_from_slice(b"\n");
        body.extend_from_slice(b"annotated tag msg\n");
        body
    }

    fn signed_tag_body() -> Vec<u8> {
        let mut body = unsigned_tag_body();
        body.extend_from_slice(b"-----BEGIN PGP SIGNATURE-----\n");
        body.extend_from_slice(b"\n");
        body.extend_from_slice(b"iQEzBAABCAAdSomeBase64Here\n");
        body.extend_from_slice(b"-----END PGP SIGNATURE-----\n");
        body
    }

    #[test]
    fn unsigned_tag_has_no_signature_offset() {
        assert_eq!(tag_signature_offset(&unsigned_tag_body()), None);
    }

    #[test]
    fn signed_tag_offset_points_at_marker_line() {
        let body = signed_tag_body();
        let offset = tag_signature_offset(&body).expect("signature offset");
        assert_eq!(&body[offset..offset + 5], b"-----");
        // Everything before the offset is the payload git echoes under -v; it must
        // end with the tag message and contain none of the signature armor.
        let payload = &body[..offset];
        assert!(payload.ends_with(b"annotated tag msg\n"));
        let payload_text = String::from_utf8(payload.to_vec()).expect("utf8");
        assert!(!payload_text.contains("BEGIN PGP SIGNATURE"));
    }

    #[test]
    fn pgp_message_marker_is_a_signature() {
        let mut body = unsigned_tag_body();
        body.extend_from_slice(b"-----BEGIN PGP MESSAGE-----\nx\n-----END PGP MESSAGE-----\n");
        assert!(tag_signature_offset(&body).is_some());
    }

    #[test]
    fn x509_and_ssh_markers_are_signatures() {
        let mut x509 = unsigned_tag_body();
        x509.extend_from_slice(
            b"-----BEGIN SIGNED MESSAGE-----\nx\n-----END SIGNED MESSAGE-----\n",
        );
        assert!(tag_signature_offset(&x509).is_some());

        let mut ssh = unsigned_tag_body();
        ssh.extend_from_slice(b"-----BEGIN SSH SIGNATURE-----\nx\n-----END SSH SIGNATURE-----\n");
        assert!(tag_signature_offset(&ssh).is_some());
    }

    #[test]
    fn gpg_signature_marker_is_not_recognized() {
        // git treats a `-----BEGIN GPG SIGNATURE-----` armor as *no* signature.
        let mut body = unsigned_tag_body();
        body.extend_from_slice(b"-----BEGIN GPG SIGNATURE-----\nx\n-----END GPG SIGNATURE-----\n");
        assert_eq!(tag_signature_offset(&body), None);
    }

    #[test]
    fn inline_marker_is_not_a_signature() {
        // A marker that is not at the start of a line lives in the message and must
        // not be treated as a signature.
        let mut body = Vec::new();
        body.extend_from_slice(b"object 3b7a93fed59b0ad9f415fcfd96addb31ac0fb752\n");
        body.extend_from_slice(b"type commit\n");
        body.extend_from_slice(b"tag v1.0\n");
        body.extend_from_slice(b"tagger Tester <tester@example.com> 1790000000 -0500\n");
        body.extend_from_slice(b"\n");
        body.extend_from_slice(b"message with -----BEGIN PGP SIGNATURE----- inline\n");
        assert_eq!(tag_signature_offset(&body), None);
    }

    #[test]
    fn marker_on_final_line_without_trailing_newline_is_found() {
        let mut body = unsigned_tag_body();
        body.extend_from_slice(b"-----BEGIN PGP SIGNATURE-----");
        assert!(tag_signature_offset(&body).is_some());
    }

    #[test]
    fn parses_verbose_raw_and_format_flags() {
        let args = vec![
            "-v".to_string(),
            "--raw".to_string(),
            "--format=%(tag)".to_string(),
            "v1.0".to_string(),
        ];
        match parse_verify_tag_args(&args).expect("parse") {
            VerifyTagInvocation::Run(options) => {
                assert!(options.verbose);
                assert!(options.raw);
                assert_eq!(options.format.as_deref(), Some("%(tag)"));
                assert_eq!(options.tags, vec!["v1.0".to_string()]);
            }
            VerifyTagInvocation::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn format_consumes_following_argument() {
        let args = vec![
            "--format".to_string(),
            "%(objecttype)".to_string(),
            "v1.0".to_string(),
        ];
        match parse_verify_tag_args(&args).expect("parse") {
            VerifyTagInvocation::Run(options) => {
                assert_eq!(options.format.as_deref(), Some("%(objecttype)"));
                assert_eq!(options.tags, vec!["v1.0".to_string()]);
            }
            VerifyTagInvocation::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn trailing_format_without_value_is_exit_129() {
        let args = vec!["--format".to_string()];
        match parse_verify_tag_args(&args) {
            Err(GitError::Exit(129)) => {}
            other => panic!("expected exit 129, got {other:?}"),
        }
    }

    #[test]
    fn double_dash_terminates_options() {
        let args = vec!["--".to_string(), "-v".to_string()];
        match parse_verify_tag_args(&args).expect("parse") {
            VerifyTagInvocation::Run(options) => {
                assert!(!options.verbose);
                assert_eq!(options.tags, vec!["-v".to_string()]);
            }
            VerifyTagInvocation::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn help_flag_requests_help() {
        let args = vec!["-h".to_string()];
        assert!(matches!(
            parse_verify_tag_args(&args).expect("parse"),
            VerifyTagInvocation::Help
        ));
    }

    #[test]
    fn unknown_long_option_is_exit_129() {
        let args = vec!["--bogus".to_string()];
        match parse_verify_tag_args(&args) {
            Err(GitError::Exit(129)) => {}
            other => panic!("expected exit 129, got {other:?}"),
        }
    }

    #[test]
    fn unknown_short_switch_is_exit_129() {
        let args = vec!["-z".to_string()];
        match parse_verify_tag_args(&args) {
            Err(GitError::Exit(129)) => {}
            other => panic!("expected exit 129, got {other:?}"),
        }
    }

    #[test]
    fn verbose_with_value_is_exit_129() {
        let args = vec!["--verbose=1".to_string()];
        match parse_verify_tag_args(&args) {
            Err(GitError::Exit(129)) => {}
            other => panic!("expected exit 129, got {other:?}"),
        }
    }
}
