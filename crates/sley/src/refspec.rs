//! Typed refspec construction for fetch/push orchestration.
//!
//! The remote APIs ([`crate::remote`]) take refspecs as rendered git-syntax
//! strings. Hand-building those strings invites silent drift (`+` placement,
//! missing `:` separator), so this module gives embedders one typed source of
//! truth: construction validates against the wire parser
//! ([`sley_protocol::parse_refspec`]), and rendering is the exact inverse of
//! parsing, so anything a [`RefSpec`] renders is something a transfer accepts.
//!
//! Negative refspecs (`^source`) exclude refs from a fetch or push. They
//! carry no destination and cannot be forced, matching upstream git.

use sley_core::{GitError, Result};
use sley_protocol::parse_refspec;

/// A Git refspec: an optional `source`, a `destination`, and a forced (`+`)
/// marker.
///
/// Construct through [`RefSpec::new`] (or the [`RefSpec::forced`] /
/// [`RefSpec::delete`] shortcuts) rather than by struct literal, so every
/// value in circulation has passed refspec validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefSpec {
    forced: bool,
    /// `None` encodes a delete refspec (`:destination`).
    source: Option<String>,
    destination: String,
}

impl RefSpec {
    /// Construct a refspec from its parts.
    ///
    /// * `source` — the refs to copy; `None` encodes a delete refspec.
    /// * `destination` — where the source lands; empty means "the same name"
    ///   and requires a `source`.
    /// * `forced` — allow a non-fast-forward update (`+` prefix).
    ///
    /// ```
    /// use sley::RefSpec;
    ///
    /// let spec = RefSpec::forced("refs/heads/*", "refs/heads/*")?;
    /// assert_eq!(spec.to_git_format(), "+refs/heads/*:refs/heads/*");
    /// # Ok::<(), sley::GitError>(())
    /// ```
    pub fn new(
        source: Option<String>,
        destination: impl Into<String>,
        forced: bool,
    ) -> Result<Self> {
        let destination = destination.into();
        if source.is_none() && destination.is_empty() {
            return Err(GitError::InvalidFormat(
                "refspec source and destination cannot both be empty".to_string(),
            ));
        }
        let spec = Self {
            forced,
            source,
            destination,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// A forced (`+`) refspec mapping `source` onto `destination`.
    pub fn forced(source: impl Into<String>, destination: impl Into<String>) -> Result<Self> {
        Self::new(Some(source.into()), destination, true)
    }

    /// A delete refspec (`:destination`). Not forced: deleting a destination
    /// that has no source cannot lose work.
    pub fn delete(destination: impl Into<String>) -> Result<Self> {
        Self::new(None, destination, false)
    }

    /// Render in git refspec syntax, including the leading `+` when forced.
    pub fn to_git_format(&self) -> String {
        format!(
            "{}{}",
            if self.forced { "+" } else { "" },
            self.to_git_format_not_forced()
        )
    }

    /// Render in git refspec syntax without the leading `+`, even when forced.
    pub fn to_git_format_not_forced(&self) -> String {
        format!(
            "{}:{}",
            self.source.as_deref().unwrap_or(""),
            self.destination
        )
    }

    /// Parse the rendered form with the wire-level parser, proving it is
    /// valid for fetch/push orchestration. Called at construction so invalid
    /// endpoints fail at the call site instead of mid-transfer.
    fn validate(&self) -> Result<()> {
        parse_refspec(&self.to_git_format()).map(|_| ())
    }
}

/// A negative refspec (`^source`) excluding refs from a fetch or push.
///
/// Private fields keep call sites on [`NegativeRefSpec::new`], so every value
/// has passed refspec validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegativeRefSpec {
    source: String,
}

impl NegativeRefSpec {
    /// Construct a negative refspec excluding `source`.
    ///
    /// Glob sources (`refs/heads/broken-*`) follow upstream git semantics and
    /// exclude every matching ref.
    pub fn new(source: impl Into<String>) -> Result<Self> {
        let spec = Self {
            source: source.into(),
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Render in git refspec syntax (`^source`).
    pub fn to_git_format(&self) -> String {
        format!("^{}", self.source)
    }

    /// See [`RefSpec::validate`].
    fn validate(&self) -> Result<()> {
        parse_refspec(&self.to_git_format()).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::{NegativeRefSpec, RefSpec};
    use crate::plumbing::sley_protocol::{parse_refspec, refspec_matches_source};

    #[test]
    fn forced_spec_renders_the_plus_prefix_and_round_trips() {
        let spec = RefSpec::forced("refs/heads/*", "refs/heads/*").expect("valid refspec");
        assert_eq!(spec.to_git_format(), "+refs/heads/*:refs/heads/*");
        assert_eq!(spec.to_git_format_not_forced(), "refs/heads/*:refs/heads/*");

        let parsed = parse_refspec(&spec.to_git_format()).expect("wire parser accepts rendering");
        assert!(parsed.force);
        assert!(parsed.pattern);
        assert_eq!(parsed.src.as_deref(), Some("refs/heads/*"));
        assert_eq!(parsed.dst.as_deref(), Some("refs/heads/*"));
    }

    #[test]
    fn plain_spec_keeps_the_force_marker_off() {
        let spec = RefSpec::new(
            Some("refs/heads/main".to_string()),
            "refs/heads/main",
            false,
        )
        .expect("valid refspec");
        assert_eq!(spec.to_git_format(), "refs/heads/main:refs/heads/main");
        assert!(!parse_refspec(&spec.to_git_format()).expect("parses").force);
    }

    #[test]
    fn empty_destination_means_the_same_name_on_the_far_side() {
        let spec =
            RefSpec::new(Some("refs/heads/main".to_string()), "", false).expect("valid refspec");
        assert_eq!(spec.to_git_format(), "refs/heads/main:");
        let parsed = parse_refspec(&spec.to_git_format()).expect("parses");
        assert_eq!(parsed.src.as_deref(), Some("refs/heads/main"));
        assert_eq!(parsed.dst, None);
    }

    #[test]
    fn delete_spec_renders_colon_prefixed_without_a_force_marker() {
        let spec = RefSpec::delete("refs/heads/old").expect("valid delete refspec");
        assert_eq!(spec.to_git_format(), ":refs/heads/old");

        let parsed = parse_refspec(&spec.to_git_format()).expect("parses");
        assert!(!parsed.force);
        assert_eq!(parsed.src, None);
        assert_eq!(parsed.dst.as_deref(), Some("refs/heads/old"));
    }

    #[test]
    fn empty_source_and_destination_is_rejected_at_construction() {
        let error = RefSpec::new(None, "", false).expect_err("both empty must fail");
        assert!(
            error
                .to_string()
                .contains("source and destination cannot both be empty"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn delimiter_bytes_are_rejected_at_construction() {
        let error = RefSpec::new(Some("refs/heads/main x".to_string()), "dst", false)
            .expect_err("space is a delimiter byte");
        assert!(
            error.to_string().contains("delimiter byte"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn unbalanced_wildcards_are_rejected_at_construction() {
        RefSpec::forced("refs/heads/*", "refs/heads/main")
            .expect_err("wildcard must appear in both source and destination");
    }

    #[test]
    fn negative_spec_renders_the_caret_prefix_and_flags_exclusion() {
        let spec = NegativeRefSpec::new("refs/heads/private").expect("valid negative refspec");
        assert_eq!(spec.to_git_format(), "^refs/heads/private");

        let parsed = parse_refspec(&spec.to_git_format()).expect("parses");
        assert!(parsed.negative);
        assert_eq!(parsed.src.as_deref(), Some("refs/heads/private"));
        // The wire type marks the exclusion; the fetch planner
        // (`fetch_refspec_excludes`) drops sources whose name matches a
        // negative refspec, so matching here is the input to exclusion.
        assert!(
            refspec_matches_source(&parsed, "refs/heads/private").expect("match check"),
            "the negative source must match by name for the planner to exclude it"
        );
    }

    #[test]
    fn negative_glob_source_follows_upstream_git_semantics() {
        let spec =
            NegativeRefSpec::new("refs/heads/broken-*").expect("glob negatives are valid git");
        let parsed = parse_refspec(&spec.to_git_format()).expect("parses");
        assert!(parsed.negative);
        assert!(parsed.pattern);
    }

    #[test]
    fn negative_spec_cannot_smuggle_a_destination_or_force_marker() {
        // No constructor accepts a destination, and the renderer owns the `^`
        // prefix, so the only failure mode left is a malformed source.
        NegativeRefSpec::new("refs/heads/bad:refs/heads/worse")
            .expect_err("colon is a delimiter byte in a negative source");
        NegativeRefSpec::new("").expect_err("empty source");
    }
}
