//! Repository-owned Unicode path precomposition (`core.precomposeunicode`).
//! Values can be copied to parallel worktree workers without changing another
//! repository's treatment of NFD directory entries or command-line paths.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// Whether this operation converts decomposed UTF-8 paths to NFC.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrecomposeUnicode(bool);

/// True if `s` contains any non-ASCII byte (git's `has_non_ascii` path check).
pub fn has_non_ascii(s: &str) -> bool {
    s.bytes().any(|b| b >= 0x80)
}

/// True if `bytes` contains any non-ASCII byte.
pub fn has_non_ascii_bytes(bytes: &[u8]) -> bool {
    bytes.iter().any(|&b| b >= 0x80)
}

impl PrecomposeUnicode {
    pub const fn new(enabled: bool) -> Self {
        Self(enabled)
    }
    pub const fn is_enabled(self) -> bool {
        self.0
    }
    /// Convert a UTF-8 string from NFD to NFC when precomposition is active.
    ///
    /// Returns the input unchanged when precomposition is off, the string is
    /// ASCII-only, invalid UTF-8 would be required, or NFC equals the input.
    pub fn string(self, input: &str) -> Cow<'_, str> {
        if !self.is_enabled() || !has_non_ascii(input) {
            return Cow::Borrowed(input);
        }
        let nfc: String = input.nfc().collect();
        if nfc == input {
            Cow::Borrowed(input)
        } else {
            Cow::Owned(nfc)
        }
    }

    /// Convert UTF-8 bytes from NFD to NFC when precomposition is active.
    ///
    /// Non-UTF-8 byte sequences are returned unchanged (git leaves illegal
    /// sequences alone rather than dying).
    pub fn bytes(self, input: &[u8]) -> Cow<'_, [u8]> {
        if !self.is_enabled() || !has_non_ascii_bytes(input) {
            return Cow::Borrowed(input);
        }
        let Ok(text) = std::str::from_utf8(input) else {
            return Cow::Borrowed(input);
        };
        match self.string(text) {
            Cow::Borrowed(_) => Cow::Borrowed(input),
            Cow::Owned(nfc) => Cow::Owned(nfc.into_bytes()),
        }
    }

    /// Precompose an owned [`String`] in place when needed.
    pub fn owned_string(self, input: String) -> String {
        match self.string(&input) {
            Cow::Borrowed(_) => input,
            Cow::Owned(nfc) => nfc,
        }
    }

    /// Precompose each component of a path when needed (for index / pathspec paths).
    pub fn path(self, path: &Path) -> Cow<'_, Path> {
        if !self.is_enabled() {
            return Cow::Borrowed(path);
        }
        let lossy = path.to_string_lossy();
        if !has_non_ascii(&lossy) {
            return Cow::Borrowed(path);
        }
        // Normalize each path component independently so separators stay platform-native
        // in the returned PathBuf while git-path `/` joins still get NFC components.
        let mut changed = false;
        let mut out = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::Normal(name) => {
                    let name_str = name.to_string_lossy();
                    match self.string(&name_str) {
                        Cow::Borrowed(_) => out.push(name),
                        Cow::Owned(nfc) => {
                            changed = true;
                            out.push(nfc);
                        }
                    }
                }
                other => out.push(other.as_os_str()),
            }
        }
        if changed {
            Cow::Owned(out)
        } else {
            Cow::Borrowed(path)
        }
    }

    /// Precompose an [`OsStr`] component for directory entries / git paths.
    pub fn os_str_bytes(self, name: &OsStr) -> Cow<'_, [u8]> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            self.bytes(name.as_bytes())
        }
        #[cfg(not(unix))]
        {
            let owned = name.to_string_lossy().into_owned();
            match self.string(&owned) {
                Cow::Borrowed(_) => Cow::Owned(owned.into_bytes()),
                Cow::Owned(nfc) => Cow::Owned(nfc.into_bytes()),
            }
        }
    }

    /// Precompose every string in `args` in place (git's `precompose_argv_prefix`).
    pub fn argv(self, args: &mut [String]) {
        if !self.is_enabled() {
            return;
        }
        for arg in args.iter_mut() {
            if has_non_ascii(arg) {
                *arg = self.owned_string(std::mem::take(arg));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfc_conversion_only_when_enabled() {
        let nfd = "A\u{0308}";
        let nfc = "\u{00c4}";
        let enabled = PrecomposeUnicode::new(true);
        let disabled = PrecomposeUnicode::new(false);
        assert_eq!(disabled.string(nfd), nfd);
        assert_eq!(enabled.string(nfd), nfc);
        assert_eq!(enabled.string(nfc), nfc);
        assert_eq!(enabled.string("ascii"), "ascii");
        assert_eq!(enabled.bytes(&[0xff]), &[0xff][..]);
    }

    #[test]
    fn path_components_are_precomposed() {
        let nfd = PathBuf::from("d.A\u{0308}/f.A\u{0308}");
        assert_eq!(
            PrecomposeUnicode::new(true).path(&nfd).to_string_lossy(),
            "d.\u{00c4}/f.\u{00c4}"
        );
    }

    #[test]
    fn workers_keep_their_own_policy() {
        let enabled = PrecomposeUnicode::new(true);
        let disabled = PrecomposeUnicode::new(false);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| enabled.string("A\u{0308}"));
            let second = scope.spawn(|| disabled.string("A\u{0308}"));
            assert_eq!(first.join().expect("NFC worker"), "\u{00c4}");
            assert_eq!(second.join().expect("NFD worker"), "A\u{0308}");
        });
    }
}
