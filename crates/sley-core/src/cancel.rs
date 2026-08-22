//! Cooperative stream cancellation.
//!
//! Sley's throughput paths are synchronous `Read`/`Write` loops. Cancellation
//! is therefore cooperative: hot loops poll a [`CancelFlag`] between units of
//! work (pack objects, compression windows, pkt-line frames, emit callbacks).
//!
//! # Design
//!
//! * [`AtomicCancel`] — shared atomic flag (UI stop, SIGINT, deadlines).
//! * [`CancelFlag`] — cheap `Copy` handle: `Option<&AtomicCancel>` (no cancel
//!   when `None`). Prefer this over generic monomorphization; every transfer
//!   seam already type-erases to one shape.
//! * [`StreamControl`] — continue/stop for callback-style event streams.
//! * [`CancellableRead`] — `Read` adapter that fails with a **dedicated**
//!   [`OperationCancelled`] payload (not [`io::ErrorKind::Interrupted`]).
//!   Using `Interrupted` is wrong: `read_exact` and sley's pkt-line readers
//!   treat it as EINTR and retry, spinning at 100% CPU after Ctrl-C.
//!
//! Pair with transport teardown ([`kill_child_if_cancelled`]) when a thread may
//! be blocked in a kernel `read` on a pipe or socket.

use crate::{GitError, Result};
use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Read};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Shared atomic cancellation flag.
///
/// Safe to share across threads via [`Arc`] or bare references. Setting the
/// flag does not interrupt a thread blocked in kernel I/O; pair with
/// [`CancellableRead`] for poll-between-reads, and [`kill_child_if_cancelled`]
/// (or closing the HTTP body) for preemptive wake-up.
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

    /// Request cancellation. Subsequent polls return `true`. Idempotent.
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

/// Cheap cooperative cancel handle for long-running I/O.
///
/// `None` means never cancel (the common default). `Some` borrows an
/// [`AtomicCancel`] owned by the CLI interrupt handler, UI, or embedder.
///
/// This is intentionally non-generic: every transfer path already needs a
/// single type-erased shape for service structs (`FetchServices`, etc.).
#[derive(Debug, Clone, Copy, Default)]
pub struct CancelFlag<'a> {
    source: Option<&'a AtomicCancel>,
}

impl CancelFlag<'static> {
    /// A flag that never reports cancellation.
    #[inline]
    pub const fn never() -> Self {
        Self { source: None }
    }
}

impl<'a> CancelFlag<'a> {
    /// Wrap a shared atomic cancel source.
    #[inline]
    pub const fn new(source: &'a AtomicCancel) -> Self {
        Self {
            source: Some(source),
        }
    }

    /// Alias for [`CancelFlag::never`] used by older call sites.
    #[inline]
    pub const fn never_dyn() -> CancelFlag<'static> {
        CancelFlag::never()
    }

    /// Whether cancellation has been requested.
    #[inline]
    pub fn is_cancelled(self) -> bool {
        self.source.is_some_and(AtomicCancel::is_cancelled)
    }

    /// Return [`GitError::Cancelled`] when the flag is set; otherwise `Ok(())`.
    #[inline]
    pub fn check(self) -> Result<()> {
        if self.is_cancelled() {
            Err(GitError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Map the flag into a [`StreamControl`] value for emit-style loops.
    #[inline]
    pub fn control(self) -> StreamControl {
        if self.is_cancelled() {
            StreamControl::Stop
        } else {
            StreamControl::Continue
        }
    }

    /// Borrowed view with a possibly shorter lifetime (no-op for `Copy`).
    #[inline]
    pub fn as_ref(self) -> CancelFlag<'a> {
        self
    }

    /// Underlying atomic, if any.
    #[inline]
    pub fn source(self) -> Option<&'a AtomicCancel> {
        self.source
    }
}

/// Historical name for the type-erased cancel handle; now identical to
/// [`CancelFlag`].
pub type DynCancelFlag<'a> = CancelFlag<'a>;

/// Marker error stored inside [`io::Error`] when a cooperative cancel fires.
///
/// **Must not** use [`io::ErrorKind::Interrupted`]: `Read::read_exact` and
/// sley's pkt-line readers treat that as EINTR and retry forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationCancelled;

impl fmt::Display for OperationCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("operation cancelled")
    }
}

impl StdError for OperationCancelled {}

/// `Read` adapter that polls a [`CancelFlag`] before each underlying read.
///
/// When cancelled, returns `Err` wrapping [`OperationCancelled`] with kind
/// [`io::ErrorKind::Other`] (never `Interrupted`).
#[derive(Debug)]
pub struct CancellableRead<'a, R> {
    inner: R,
    cancel: CancelFlag<'a>,
}

impl<'a, R> CancellableRead<'a, R> {
    /// Wrap `inner`, polling `cancel` before every read.
    #[inline]
    pub fn new(inner: R, cancel: CancelFlag<'a>) -> Self {
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
    pub fn cancel(&self) -> CancelFlag<'a> {
        self.cancel
    }
}

impl<R: Read> Read for CancellableRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(cancelled_io_error());
        }
        self.inner.read(buf)
    }
}

/// Build the I/O error used by [`CancellableRead`] on cancel.
///
/// Kind is **`Other`**, payload is [`OperationCancelled`]. Do not use
/// `Interrupted` — see that type's docs.
#[inline]
pub fn cancelled_io_error() -> io::Error {
    io::Error::other(OperationCancelled)
}

/// `true` when `err` is a cooperative cancel from [`CancellableRead`].
#[inline]
pub fn is_cancelled_io(err: &io::Error) -> bool {
    err.get_ref()
        .is_some_and(|inner| inner.downcast_ref::<OperationCancelled>().is_some())
}

/// Map a cancel-flavored I/O error to [`GitError::Cancelled`]; pass other
/// errors through [`GitError::from`].
#[inline]
pub fn map_cancel_io(err: io::Error) -> GitError {
    if is_cancelled_io(&err) {
        GitError::Cancelled
    } else {
        GitError::from(err)
    }
}

/// True when `err` is a cooperative cancellation.
///
/// Delegates to [`GitError::is_cancelled`], which uniformly covers the
/// explicit `Cancelled` variant and cancel-flavored I/O errors.
#[inline]
pub fn is_cancelled_error(err: &GitError) -> bool {
    err.is_cancelled()
}

/// Kill an OS child if cancel was requested (best-effort preemptive wake).
#[inline]
pub fn kill_child_if_cancelled(child: &mut Child, cancel: CancelFlag<'_>) {
    if cancel.is_cancelled() {
        let _ = child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

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
    fn cancellable_read_fails_with_operation_cancelled_not_interrupted() {
        let source = AtomicCancel::new();
        let data = b"hello world";
        let mut reader = CancellableRead::new(Cursor::new(&data[..]), CancelFlag::new(&source));
        let mut buf = [0u8; 5];
        assert_eq!(reader.read(&mut buf).expect("read"), 5);
        source.cancel();
        let err = reader.read(&mut buf).expect_err("cancelled");
        assert_ne!(
            err.kind(),
            io::ErrorKind::Interrupted,
            "Interrupted is retried by read_exact / pkt-line"
        );
        assert!(is_cancelled_io(&err));
        assert_eq!(map_cancel_io(err), GitError::Cancelled);
    }

    #[test]
    fn read_exact_does_not_spin_on_cancel() {
        let source = AtomicCancel::new();
        source.cancel();
        let mut reader = CancellableRead::new(Cursor::new(&b"abcd"[..]), CancelFlag::new(&source));
        let mut buf = [0u8; 4];
        // Would spin forever if cancel used ErrorKind::Interrupted.
        let err = reader.read_exact(&mut buf).expect_err("cancelled");
        assert!(is_cancelled_io(&err));
    }

    #[test]
    fn never_dyn_alias_matches_never() {
        assert!(!CancelFlag::never().is_cancelled());
    }

    #[test]
    fn kill_child_if_cancelled_is_noop_when_not_cancelled() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let source = AtomicCancel::new();
        kill_child_if_cancelled(&mut child, CancelFlag::new(&source));
        assert!(child.try_wait().expect("try_wait").is_none());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn kill_child_if_cancelled_kills_when_flag_set() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let source = AtomicCancel::new();
        source.cancel();
        kill_child_if_cancelled(&mut child, CancelFlag::new(&source));
        let status = child.wait().expect("wait after kill");
        assert!(!status.success());
    }
}
