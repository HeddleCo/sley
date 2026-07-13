//! Cooperative stream cancellation.
//!
//! Sley's throughput paths are synchronous `Read`/`Write` loops. Cancellation
//! is therefore cooperative: hot loops poll a [`CancelFlag`] between units of
//! work (pack objects, compression windows, pkt-line frames, emit callbacks).
//!
//! # Design
//!
//! * [`Cancel`] — trait for a cancellation source.
//! * [`CancelFlag<C>`] — monomorphized handle over any `C: Cancel`. Prefer this
//!   over `dyn Cancel` on hot paths so `Never` checks optimize away.
//! * [`Never`] — zero-cost default source that is never cancelled.
//! * [`AtomicCancel`] — shared flag for cross-thread cancel (UI stop, SIGINT).
//! * [`StreamControl`] — continue/stop for callback-style event streams.
//! * [`CancellableRead`] — `Read` adapter that fails with
//!   [`std::io::ErrorKind::Interrupted`] when the flag trips between reads.
//!
//! Type erasure is still available when needed: `CancelFlag<&dyn Cancel>` or
//! `CancelFlag<Arc<dyn Cancel + Send + Sync>>` via the blanket [`Cancel`] impls
//! for references and `Arc`.

use crate::{GitError, Result};
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cooperative continue/stop control for callback-style event streams
/// (status rows, revwalk commits, untracked paths, …).
///
/// Distinct from [`GitError::Cancelled`]: `Stop` is a successful early exit
/// requested by the consumer; `Cancelled` is an error from an external cancel
/// source (SIGINT, UI stop, deadline).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamControl {
    #[default]
    Continue,
    Stop,
}

impl StreamControl {
    /// `true` when the consumer requested an early stop.
    #[inline]
    pub const fn is_stop(self) -> bool {
        matches!(self, Self::Stop)
    }

    /// `true` when the consumer wants more items.
    #[inline]
    pub const fn is_continue(self) -> bool {
        matches!(self, Self::Continue)
    }
}

/// A cooperative cancellation source.
///
/// Implementations must be cheap to poll. Hot paths call [`Cancel::is_cancelled`]
/// frequently; monomorphization over concrete types keeps the never-cancel case
/// essentially free.
pub trait Cancel {
    /// `true` once cancellation has been requested.
    fn is_cancelled(&self) -> bool;
}

/// Zero-cost cancellation source that is never cancelled.
///
/// This is the default type parameter of [`CancelFlag`]. Use
/// [`CancelFlag::never`] at call sites that do not support cancel.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Never;

impl Cancel for Never {
    #[inline(always)]
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// Shared atomic cancellation flag.
///
/// Safe to share across threads via [`Arc`] or bare references. Setting the
/// flag does not interrupt a thread blocked in kernel I/O; pair with
/// [`CancellableRead`] for poll-between-reads, or close the underlying
/// transport for preemptive wake-up.
#[derive(Debug, Default)]
pub struct AtomicCancel {
    cancelled: AtomicBool,
}

impl AtomicCancel {
    /// Create a flag that is not yet cancelled.
    #[inline]
    pub const fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
        }
    }

    /// Request cancellation. Subsequent [`Cancel::is_cancelled`] polls return
    /// `true`. Idempotent.
    #[inline]
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Clear the flag so a subsequent operation can reuse this source.
    #[inline]
    pub fn clear(&self) {
        self.cancelled.store(false, Ordering::Release);
    }

    /// Current flag value.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl Cancel for AtomicCancel {
    #[inline]
    fn is_cancelled(&self) -> bool {
        AtomicCancel::is_cancelled(self)
    }
}

impl Cancel for AtomicBool {
    #[inline]
    fn is_cancelled(&self) -> bool {
        self.load(Ordering::Acquire)
    }
}

impl<C: Cancel + ?Sized> Cancel for &C {
    #[inline]
    fn is_cancelled(&self) -> bool {
        (**self).is_cancelled()
    }
}

impl<C: Cancel + ?Sized> Cancel for Arc<C> {
    #[inline]
    fn is_cancelled(&self) -> bool {
        (**self).is_cancelled()
    }
}

/// Monomorphized cancellation handle.
///
/// Prefer a concrete `C` (especially [`Never`]) on hot paths. Use
/// `CancelFlag<&dyn Cancel>` only at type-erased boundaries (trait objects,
/// heterogeneous storage).
#[derive(Debug, Clone, Copy, Default)]
pub struct CancelFlag<C: Cancel = Never> {
    source: C,
}

impl CancelFlag<Never> {
    /// A flag that never reports cancellation.
    #[inline]
    pub const fn never() -> Self {
        Self { source: Never }
    }

    /// Type-erased never-cancel handle for long-lived service structs that
    /// cannot monomorphize over a concrete `C: Cancel` (e.g. `FetchServices`).
    ///
    /// Prefer [`CancelFlag::never`] on hot paths so monomorphization can
    /// eliminate the poll entirely.
    #[inline]
    pub fn never_dyn() -> CancelFlag<&'static (dyn Cancel + Send + Sync)> {
        static NEVER: Never = Never;
        CancelFlag {
            source: &NEVER as &'static (dyn Cancel + Send + Sync),
        }
    }
}

/// Type-erased cancel handle for orchestration seams (fetch/clone/push
/// services) that store one cancel source for many monomorphized install paths.
///
/// Requires `Send + Sync` so the flag can be shared into scoped pack-generator
/// threads (e.g. HTTP push body streaming).
pub type DynCancelFlag<'a> = CancelFlag<&'a (dyn Cancel + Send + Sync)>;

impl<C: Cancel> CancelFlag<C> {
    /// Wrap a cancellation source.
    #[inline]
    pub const fn new(source: C) -> Self {
        Self { source }
    }

    /// Borrow the inner source as a nested flag (for passing down without
    /// cloning owned sources like [`Arc`]).
    #[inline]
    pub fn as_ref(&self) -> CancelFlag<&C> {
        CancelFlag {
            source: &self.source,
        }
    }

    /// Access the underlying source.
    #[inline]
    pub fn source(&self) -> &C {
        &self.source
    }

    /// Unwrap the underlying source.
    #[inline]
    pub fn into_source(self) -> C {
        self.source
    }

    /// Whether cancellation has been requested.
    #[inline]
    fn is_cancelled_inner(&self) -> bool {
        self.source.is_cancelled()
    }

    /// Whether cancellation has been requested.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled_inner()
    }

    /// Return [`GitError::Cancelled`] when the flag is set; otherwise `Ok(())`.
    #[inline]
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled_inner() {
            Err(GitError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Map the flag into a [`StreamControl`] value for emit-style loops.
    ///
    /// Cancelled → [`StreamControl::Stop`]; otherwise [`StreamControl::Continue`].
    #[inline]
    pub fn control(&self) -> StreamControl {
        if self.is_cancelled_inner() {
            StreamControl::Stop
        } else {
            StreamControl::Continue
        }
    }
}

impl<C: Cancel> Cancel for CancelFlag<C> {
    #[inline]
    fn is_cancelled(&self) -> bool {
        self.is_cancelled_inner()
    }
}

/// `Read` adapter that polls a [`CancelFlag`] before each underlying read.
///
/// When cancelled, returns `Err` with [`io::ErrorKind::Interrupted`]. Callers
/// that map I/O errors through [`GitError`] should treat Interrupted as
/// [`GitError::Cancelled`] (see [`map_cancel_io`]).
#[derive(Debug)]
pub struct CancellableRead<R, C: Cancel = Never> {
    inner: R,
    cancel: CancelFlag<C>,
}

impl<R, C: Cancel> CancellableRead<R, C> {
    /// Wrap `inner`, polling `cancel` before every read.
    #[inline]
    pub fn new(inner: R, cancel: CancelFlag<C>) -> Self {
        Self { inner, cancel }
    }

    /// Borrow the inner reader.
    #[inline]
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Mutably borrow the inner reader.
    #[inline]
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Unwrap the inner reader.
    #[inline]
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// The cancel flag driving this adapter.
    #[inline]
    pub fn cancel(&self) -> CancelFlag<&C> {
        self.cancel.as_ref()
    }
}

impl<R: Read, C: Cancel> Read for CancellableRead<R, C> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(cancelled_io_error());
        }
        self.inner.read(buf)
    }
}

/// Build the standard I/O error used by [`CancellableRead`] on cancel.
#[inline]
pub fn cancelled_io_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "operation cancelled")
}

/// Map a cancel-flavored I/O error to [`GitError::Cancelled`]; pass other
/// errors through [`GitError::from`].
#[inline]
pub fn map_cancel_io(err: io::Error) -> GitError {
    if err.kind() == io::ErrorKind::Interrupted
        && err
            .get_ref()
            .map(|inner| inner.to_string() == "operation cancelled")
            .unwrap_or(false)
    {
        GitError::Cancelled
    } else if err.kind() == io::ErrorKind::Interrupted && err.to_string().contains("cancelled") {
        GitError::Cancelled
    } else {
        GitError::from(err)
    }
}

/// True when `err` is a cooperative cancellation.
#[inline]
pub fn is_cancelled_error(err: &GitError) -> bool {
    matches!(err, GitError::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn never_flag_is_never_cancelled() {
        let flag = CancelFlag::never();
        assert!(!flag.is_cancelled());
        assert!(flag.check().is_ok());
        assert_eq!(flag.control(), StreamControl::Continue);
    }

    #[test]
    fn atomic_cancel_trips_flag() {
        let source = AtomicCancel::new();
        let flag = CancelFlag::new(&source);
        assert!(!flag.is_cancelled());
        source.cancel();
        assert!(flag.is_cancelled());
        assert_eq!(flag.check(), Err(GitError::Cancelled));
        assert_eq!(flag.control(), StreamControl::Stop);
        source.clear();
        assert!(!flag.is_cancelled());
    }

    #[test]
    fn arc_atomic_cancel_shares_state() {
        let source = Arc::new(AtomicCancel::new());
        let flag_a = CancelFlag::new(Arc::clone(&source));
        let flag_b = CancelFlag::new(Arc::clone(&source));
        source.cancel();
        assert!(flag_a.is_cancelled());
        assert!(flag_b.is_cancelled());
    }

    #[test]
    fn dyn_cancel_type_erasure_works() {
        let source = AtomicCancel::new();
        let erased: &dyn Cancel = &source;
        let flag = CancelFlag::new(erased);
        assert!(!flag.is_cancelled());
        source.cancel();
        assert!(flag.is_cancelled());
    }

    #[test]
    fn cancellable_read_fails_when_flag_set() {
        let source = AtomicCancel::new();
        let data = b"hello world";
        let mut reader = CancellableRead::new(Cursor::new(&data[..]), CancelFlag::new(&source));
        let mut buf = [0u8; 5];
        assert_eq!(reader.read(&mut buf).expect("read"), 5);
        source.cancel();
        let err = reader.read(&mut buf).expect_err("cancelled");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert_eq!(map_cancel_io(err), GitError::Cancelled);
    }

    #[test]
    fn cancel_flag_as_ref_preserves_state() {
        let source = AtomicCancel::new();
        let owned = CancelFlag::new(&source);
        let borrowed = owned.as_ref();
        source.cancel();
        assert!(borrowed.is_cancelled());
    }

    #[test]
    fn never_dyn_is_never_cancelled_and_accepts_atomic() {
        let dyn_never = CancelFlag::never_dyn();
        assert!(!dyn_never.is_cancelled());
        assert!(dyn_never.check().is_ok());

        let source = AtomicCancel::new();
        let dyn_flag: DynCancelFlag<'_> =
            CancelFlag::new(&source as &(dyn Cancel + Send + Sync));
        assert!(!dyn_flag.is_cancelled());
        source.cancel();
        assert!(dyn_flag.is_cancelled());
        assert_eq!(dyn_flag.check(), Err(GitError::Cancelled));
    }
}
