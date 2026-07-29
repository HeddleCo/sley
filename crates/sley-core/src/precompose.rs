//! macOS HFS+/APFS Unicode path precomposition (`core.precomposeunicode`).
//!
//! On Apple filesystems, directory entries are typically stored in NFD while
//! Git wants NFC path names in the index and trees. When
//! `core.precomposeunicode` is true, Git:
//!
//! 1. Converts NFD command-line paths to NFC (`precompose_string_if_needed`)
//! 2. Converts NFD names from `readdir` to NFC (`precompose_utf8_readdir`)
//!
//! This module mirrors that behavior for Sley. Conversion is gated by a
//! process-wide flag set from the effective repository config (same role as
//! git's `precomposed_unicode` cache). The flag is shared across threads so
//! parallel worktree/status workers see the same value as the main thread.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use unicode_normalization::UnicodeNormalization;

/// Whether the active repository has `core.precomposeunicode=true`.
/// Matches git's cached `repo_config_values::precomposed_unicode == 1`.
/// Process-wide so status/worktree worker threads share the repo-open value.
static PRECOMPOSE_UNICODE: AtomicBool = AtomicBool::new(false);

/// Enable or disable NFD→NFC path conversion for this process.
pub fn set_precompose_unicode(enabled: bool) {
    PRECOMPOSE_UNICODE.store(enabled, Ordering::Relaxed);
}

/// Whether path precomposition is currently active.
pub fn precompose_unicode_enabled() -> bool {
    PRECOMPOSE_UNICODE.load(Ordering::Relaxed)
}

/// Activate precomposition from a config bool (or `None` → disabled).
pub fn activate_precompose_unicode(value: Option<bool>) {
    set_precompose_unicode(value.unwrap_or(false));
}

/// True if `s` contains any non-ASCII byte (git's `has_non_ascii` path check).
pub fn has_non_ascii(s: &str) -> bool {
    s.bytes().any(|b| b >= 0x80)
}

/// True if `bytes` contains any non-ASCII byte.
pub fn has_non_ascii_bytes(bytes: &[u8]) -> bool {
    bytes.iter().any(|&b| b >= 0x80)
}

/// Convert a UTF-8 string from NFD to NFC when precomposition is active.
///
/// Returns the input unchanged when precomposition is off, the string is
/// ASCII-only, invalid UTF-8 would be required, or NFC equals the input.
pub fn precompose_string_if_needed(input: &str) -> Cow<'_, str> {
    if !precompose_unicode_enabled() || !has_non_ascii(input) {
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
pub fn precompose_bytes_if_needed(input: &[u8]) -> Cow<'_, [u8]> {
    if !precompose_unicode_enabled() || !has_non_ascii_bytes(input) {
        return Cow::Borrowed(input);
    }
    let Ok(text) = std::str::from_utf8(input) else {
        return Cow::Borrowed(input);
    };
    match precompose_string_if_needed(text) {
        Cow::Borrowed(_) => Cow::Borrowed(input),
        Cow::Owned(nfc) => Cow::Owned(nfc.into_bytes()),
    }
}

/// Precompose an owned [`String`] in place when needed.
pub fn precompose_owned_string_if_needed(input: String) -> String {
    match precompose_string_if_needed(&input) {
        Cow::Borrowed(_) => input,
        Cow::Owned(nfc) => nfc,
    }
}

/// Precompose each component of a path when needed (for index / pathspec paths).
pub fn precompose_path_if_needed(path: &Path) -> Cow<'_, Path> {
    if !precompose_unicode_enabled() {
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
                match precompose_string_if_needed(&name_str) {
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
pub fn precompose_os_str_bytes_if_needed(name: &OsStr) -> Cow<'_, [u8]> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        precompose_bytes_if_needed(name.as_bytes())
    }
    #[cfg(not(unix))]
    {
        let owned = name.to_string_lossy().into_owned();
        match precompose_string_if_needed(&owned) {
            Cow::Borrowed(_) => Cow::Owned(owned.into_bytes()),
            Cow::Owned(nfc) => Cow::Owned(nfc.into_bytes()),
        }
    }
}

/// Precompose every string in `args` in place (git's `precompose_argv_prefix`).
pub fn precompose_argv_if_needed(args: &mut [String]) {
    if !precompose_unicode_enabled() {
        return;
    }
    for arg in args.iter_mut() {
        if has_non_ascii(arg) {
            *arg = precompose_owned_string_if_needed(std::mem::take(arg));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Serialize tests that mutate the process-wide precompose flag so they
    /// do not race when cargo runs the suite with multiple threads.
    fn lock_precompose_for_test() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn nfc_conversion_only_when_enabled() {
        let _guard = lock_precompose_for_test();
        let nfd = "A\u{0308}"; // A + combining diaeresis
        let nfc = "\u{00C4}"; // Ä

        set_precompose_unicode(false);
        assert_eq!(precompose_string_if_needed(nfd).as_ref(), nfd);

        set_precompose_unicode(true);
        assert_eq!(precompose_string_if_needed(nfd).as_ref(), nfc);
        assert_eq!(precompose_string_if_needed(nfc).as_ref(), nfc);
        assert_eq!(precompose_string_if_needed("ascii").as_ref(), "ascii");

        set_precompose_unicode(false);
    }

    #[test]
    fn path_components_are_precomposed() {
        let _guard = lock_precompose_for_test();
        set_precompose_unicode(true);
        let nfd = PathBuf::from("d.A\u{0308}/f.A\u{0308}".to_string());
        let precomposed = precompose_path_if_needed(&nfd);
        assert_eq!(
            precomposed.to_string_lossy().as_ref(),
            "d.\u{00C4}/f.\u{00C4}"
        );
        set_precompose_unicode(false);
    }

    /// Parallel status workers must observe the same flag the parent set at
    /// repo open (no per-thread re-activation required).
    #[test]
    fn worker_thread_sees_parent_set_value() {
        let _guard = lock_precompose_for_test();
        set_precompose_unicode(true);
        let nfd = "A\u{0308}";
        let nfc = "\u{00C4}";

        let enabled = std::thread::scope(|s| {
            s.spawn(|| {
                assert!(precompose_unicode_enabled());
                assert_eq!(precompose_string_if_needed(nfd).as_ref(), nfc);
                precompose_unicode_enabled()
            })
            .join()
            .expect("worker join")
        });
        assert!(enabled);

        set_precompose_unicode(false);
        let disabled = std::thread::scope(|s| {
            s.spawn(|| {
                assert!(!precompose_unicode_enabled());
                assert_eq!(precompose_string_if_needed(nfd).as_ref(), nfd);
                precompose_unicode_enabled()
            })
            .join()
            .expect("worker join")
        });
        assert!(!disabled);
    }
}
