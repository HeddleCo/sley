//! Process-level cooperative cancel driven by Ctrl-C / SIGINT.
//!
//! Uses the `ctrlc` crate (signal handling lives outside this crate's
//! `forbid(unsafe_code)` boundary). The first call installs a process-wide
//! handler that trips a shared [`AtomicCancel`]; later calls reuse it.
//!
//! Fetch/clone/push pass the flag into `sley_remote` services so pack
//! receive/generate can stop cooperatively with [`GitError::Cancelled`]
//! (CLI exit 130) instead of only dying mid-syscall.

use sley::{AtomicCancel, Cancel, CancelFlag, DynCancelFlag};
use std::sync::{Arc, OnceLock};

static PROCESS_CANCEL: OnceLock<Arc<AtomicCancel>> = OnceLock::new();

/// Shared process cancel source. Installs a Ctrl-C handler on first use.
///
/// Callers should keep the returned [`Arc`] alive for the duration of the
/// operation and build a [`DynCancelFlag`] via [`dyn_cancel_flag`].
pub(crate) fn process_interrupt_cancel() -> Arc<AtomicCancel> {
    PROCESS_CANCEL
        .get_or_init(|| {
            let flag = Arc::new(AtomicCancel::new());
            let for_handler = Arc::clone(&flag);
            // Best-effort: if a handler is already installed (tests / embedders),
            // the AtomicCancel is still usable when set from other code.
            let _ = ctrlc::set_handler(move || {
                for_handler.cancel();
            });
            flag
        })
        .clone()
}

/// Clear any previous cancel request so a subsequent long-running command can
/// start cleanly in the same process (test runners, multi-command CLI).
pub(crate) fn reset_process_interrupt_cancel(flag: &AtomicCancel) {
    flag.clear();
}

/// Type-erased cancel handle for `FetchServices` / `CloneServices` / `PushServices`.
pub(crate) fn dyn_cancel_flag(flag: &AtomicCancel) -> DynCancelFlag<'_> {
    CancelFlag::new(flag as &(dyn Cancel + Send + Sync))
}
