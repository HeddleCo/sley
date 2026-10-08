//! Git refname validation: a byte-exact port of `check_refname_format`
//! (`refs.c`), the rule set behind `git check-ref-format`.
//!
//! Every ref-name check in sley should route through [`check_refname_format`]
//! so that sley accepts exactly the names Git accepts. Git's rules are
//! byte-oriented and ASCII-only: any byte `>= 0x80` is an ordinary refname
//! byte, so non-ASCII whitespace such as U+00A0 (NO-BREAK SPACE) is valid
//! anywhere in a name, including at its edges.
//!
//! The rules, per `/`-separated component:
//! - it must not be empty (so no leading or trailing `/` and no `//`);
//! - it must not start with `.` or end with `.lock`;
//! - it must not contain `..` or `@{`;
//! - it must not contain an ASCII control byte (`< 0x20`), DEL (`0x7f`), space,
//!   or any of `~ ^ : ? [ \`;
//! - it must not contain `*`, unless [`RefnameFormat::refspec_pattern`] is set,
//!   in which case exactly one `*` is allowed in the whole name.
//!
//! And for the whole name: it must not be the single character `@`, must not
//! end with `.`, and must have at least two components unless
//! [`RefnameFormat::allow_onelevel`] is set.
//!
//! A NUL byte, which terminates the name in Git's C implementation and so can
//! never reach it, is rejected as a control byte.

use std::fmt;

/// Flags for [`check_refname_format`], mirroring git's `REFNAME_*` flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RefnameFormat {
    /// `REFNAME_ALLOW_ONELEVEL`: accept a name with a single component, such as
    /// `HEAD` or `FETCH_HEAD`.
    pub allow_onelevel: bool,
    /// `REFNAME_REFSPEC_PATTERN`: accept a single `*` anywhere in the name.
    pub refspec_pattern: bool,
}

impl RefnameFormat {
    /// `check_refname_format(name, 0)`: at least two components, no `*`.
    pub const STRICT: Self = Self {
        allow_onelevel: false,
        refspec_pattern: false,
    };

    /// `check_refname_format(name, REFNAME_ALLOW_ONELEVEL)`.
    pub const ALLOW_ONELEVEL: Self = Self {
        allow_onelevel: true,
        refspec_pattern: false,
    };
}

/// Why a name failed [`check_refname_format`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RefnameFormatError {
    /// The name is the single character `@`.
    LoneAt,
    /// A component is empty: the name is empty, or has a leading, trailing,
    /// or doubled `/`.
    EmptyComponent,
    /// A component starts with `.`.
    ComponentStartsWithDot,
    /// A component ends with `.lock`.
    ComponentEndsWithLock,
    /// The name contains `..`.
    DoubleDot,
    /// The name contains `@{`.
    AtBrace,
    /// The name contains a forbidden byte: an ASCII control byte, DEL, space,
    /// or one of `~ ^ : ? [ \`.
    ForbiddenByte(u8),
    /// The name contains `*` outside a refspec pattern, or more than one `*`.
    Asterisk,
    /// The name ends with `.`.
    EndsWithDot,
    /// The name has a single component and one-level names were not allowed.
    OneLevel,
}

impl fmt::Display for RefnameFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoneAt => f.write_str("ref name must not be the single character '@'"),
            Self::EmptyComponent => f.write_str(
                "ref name must not be empty or have a leading, trailing, or consecutive slash",
            ),
            Self::ComponentStartsWithDot => {
                f.write_str("ref name component must not start with '.'")
            }
            Self::ComponentEndsWithLock => {
                f.write_str("ref name component must not end with '.lock'")
            }
            Self::DoubleDot => f.write_str("ref name must not contain '..'"),
            Self::AtBrace => f.write_str("ref name must not contain '@{'"),
            Self::ForbiddenByte(b' ') => f.write_str("ref name must not contain a space"),
            Self::ForbiddenByte(byte) if byte.is_ascii_control() => write!(
                f,
                "ref name must not contain control character 0x{byte:02x}"
            ),
            Self::ForbiddenByte(byte) => {
                write!(f, "ref name must not contain '{}'", char::from(*byte))
            }
            Self::Asterisk => {
                f.write_str("ref name must not contain '*' (a refspec pattern allows exactly one)")
            }
            Self::EndsWithDot => f.write_str("ref name must not end with '.'"),
            Self::OneLevel => f.write_str("ref name must have at least two components"),
        }
    }
}

impl std::error::Error for RefnameFormatError {}

/// Validate `name` exactly as git's `check_refname_format(name, flags)` does.
///
/// Takes bytes because Git refnames are byte strings; pass `str::as_bytes` for
/// UTF-8 names.
pub fn check_refname_format(name: &[u8], format: RefnameFormat) -> Result<(), RefnameFormatError> {
    if name == b"@" {
        return Err(RefnameFormatError::LoneAt);
    }
    let mut pattern_allowed = format.refspec_pattern;
    let mut components = 0usize;
    for component in name.split(|&byte| byte == b'/') {
        check_refname_component(component, &mut pattern_allowed)?;
        components += 1;
    }
    if name.last() == Some(&b'.') {
        return Err(RefnameFormatError::EndsWithDot);
    }
    if !format.allow_onelevel && components < 2 {
        return Err(RefnameFormatError::OneLevel);
    }
    Ok(())
}

/// git's `check_refname_component` with its `refname_disposition` table.
/// `pattern_allowed` is the shared `REFNAME_REFSPEC_PATTERN` flag, cleared by
/// the first `*` so a second one anywhere in the name is rejected.
fn check_refname_component(
    component: &[u8],
    pattern_allowed: &mut bool,
) -> Result<(), RefnameFormatError> {
    let mut last = 0u8;
    for &byte in component {
        match byte {
            b'.' if last == b'.' => return Err(RefnameFormatError::DoubleDot),
            b'{' if last == b'@' => return Err(RefnameFormatError::AtBrace),
            b'*' => {
                if !*pattern_allowed {
                    return Err(RefnameFormatError::Asterisk);
                }
                *pattern_allowed = false;
            }
            0x00..=0x20 | 0x7f | b'~' | b'^' | b':' | b'?' | b'[' | b'\\' => {
                return Err(RefnameFormatError::ForbiddenByte(byte));
            }
            _ => {}
        }
        last = byte;
    }
    if component.is_empty() {
        return Err(RefnameFormatError::EmptyComponent);
    }
    if component.first() == Some(&b'.') {
        return Err(RefnameFormatError::ComponentStartsWithDot);
    }
    if component.ends_with(b".lock") {
        return Err(RefnameFormatError::ComponentEndsWithLock);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &[u8]) -> Result<(), RefnameFormatError> {
        check_refname_format(name, RefnameFormat::STRICT)
    }

    #[test]
    fn non_ascii_whitespace_is_an_ordinary_byte() {
        // HeddleCo/sley#244: Git accepts NBSP (and every other non-ASCII
        // whitespace) anywhere, including at the edges of the name.
        assert_eq!(check("refs/heads/\u{00A0}edge\u{00A0}".as_bytes()), Ok(()));
        assert_eq!(check("\u{3000}refs/heads/x\u{2028}".as_bytes()), Ok(()));
        assert_eq!(
            check(b"refs/heads/ edge"),
            Err(RefnameFormatError::ForbiddenByte(b' '))
        );
    }

    #[test]
    fn non_utf8_bytes_are_ordinary() {
        assert_eq!(check(b"refs/heads/\xff\xfe"), Ok(()));
    }

    #[test]
    fn nul_is_rejected_as_a_control_byte() {
        assert_eq!(
            check(b"refs/heads/a\0b"),
            Err(RefnameFormatError::ForbiddenByte(0))
        );
    }

    #[test]
    fn structural_rules() {
        assert_eq!(
            check_refname_format(b"@", RefnameFormat::ALLOW_ONELEVEL),
            Err(RefnameFormatError::LoneAt)
        );
        assert_eq!(check(b""), Err(RefnameFormatError::EmptyComponent));
        assert_eq!(check(b"refs//a"), Err(RefnameFormatError::EmptyComponent));
        assert_eq!(check(b"refs/a/"), Err(RefnameFormatError::EmptyComponent));
        assert_eq!(
            check(b"refs/.a"),
            Err(RefnameFormatError::ComponentStartsWithDot)
        );
        assert_eq!(
            check(b"refs/a.lock/b"),
            Err(RefnameFormatError::ComponentEndsWithLock)
        );
        assert_eq!(check(b"refs/a..b"), Err(RefnameFormatError::DoubleDot));
        assert_eq!(check(b"refs/a@{b"), Err(RefnameFormatError::AtBrace));
        assert_eq!(check(b"refs/a."), Err(RefnameFormatError::EndsWithDot));
        assert_eq!(check(b"refs/a./b"), Ok(()));
        assert_eq!(check(b"HEAD"), Err(RefnameFormatError::OneLevel));
        assert_eq!(
            check_refname_format(b"HEAD", RefnameFormat::ALLOW_ONELEVEL),
            Ok(())
        );
    }

    #[test]
    fn refspec_pattern_allows_exactly_one_asterisk() {
        let pattern = RefnameFormat {
            allow_onelevel: false,
            refspec_pattern: true,
        };
        assert_eq!(check(b"refs/heads/*"), Err(RefnameFormatError::Asterisk));
        assert_eq!(check_refname_format(b"refs/heads/*", pattern), Ok(()));
        assert_eq!(
            check_refname_format(b"refs/*/a*", pattern),
            Err(RefnameFormatError::Asterisk)
        );
    }
}
