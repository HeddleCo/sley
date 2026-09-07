//! Caller-owned diagnostics for synchronous operations.
//!
//! A scope routes library messages to the caller without changing process output.
//! Scopes nest and unwind safely, and are isolated between threads. A worker must
//! explicitly inherit its parent's scope with [`inherit`]. Async callers should
//! install a scope inside their blocking operation, never around future creation
//! or across an await. Protocol bodies and explicit `Write` parameters are separate.

use std::cell::RefCell;
use std::fmt;
use std::io;
use std::sync::Arc;

/// Git's two human-facing output channels. Bytes include their original terminators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticStream {
    Stdout,
    Stderr,
}

/// A caller's renderer, collector, or logger. Implementations may be called by
/// several workers concurrently. No library-provided sink writes to the process.
pub trait DiagnosticSink: Send + Sync {
    fn write(&self, stream: DiagnosticStream, bytes: &[u8]) -> io::Result<()>;
    fn flush(&self, _stream: DiagnosticStream) -> io::Result<()> {
        Ok(())
    }
}

/// A shareable caller-owned sink; the default discards diagnostics.
/// Dynamic dispatch is confined to this application-provided rendering boundary.
#[derive(Clone, Default)]
pub struct Diagnostics(Option<Arc<dyn DiagnosticSink>>);

thread_local! {
    // Routing only: no repository configuration or process-wide default/setter.
    static ACTIVE: RefCell<Diagnostics> = RefCell::new(Diagnostics::default());
}

impl Diagnostics {
    pub fn new(sink: impl DiagnosticSink + 'static) -> Self {
        Self(Some(Arc::new(sink)))
    }

    pub fn current() -> Self {
        ACTIVE.with(|active| active.borrow().clone())
    }

    /// Run a synchronous operation with this sink, restoring the previous scope
    /// on both normal return and unwinding. The sink may reenter the library.
    pub fn scope<T>(&self, operation: impl FnOnce() -> T) -> T {
        struct Restore(Diagnostics);
        impl Drop for Restore {
            fn drop(&mut self) {
                ACTIVE.with(|active| *active.borrow_mut() = self.0.clone());
            }
        }
        let _restore = Restore(ACTIVE.with(|active| active.replace(self.clone())));
        operation()
    }

    pub fn write(&self, stream: DiagnosticStream, bytes: &[u8]) -> io::Result<()> {
        match &self.0 {
            Some(sink) => sink.write(stream, bytes),
            None => Ok(()),
        }
    }

    pub fn flush(&self, stream: DiagnosticStream) -> io::Result<()> {
        match &self.0 {
            Some(sink) => sink.flush(stream),
            None => Ok(()),
        }
    }
}

/// Capture the current operation's sink before spawning a synchronous worker.
pub fn inherit<T>(operation: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let diagnostics = Diagnostics::current();
    move || diagnostics.scope(operation)
}

/// Emit a complete formatted record. As with Git's best-effort diagnostics,
/// failures do not replace the operation's typed error. Fallible output paths
/// should use [`DiagnosticWriter`] or [`Diagnostics::write`] instead.
pub fn emit(stream: DiagnosticStream, args: fmt::Arguments<'_>, newline: bool) {
    let diagnostics = Diagnostics::current();
    if diagnostics.0.is_none() {
        return;
    }
    let mut bytes = args.to_string().into_bytes();
    if newline {
        bytes.push(b'\n');
    }
    let _ = diagnostics.write(stream, &bytes);
}

/// `Write` adapter bound to the current caller's sink at construction.
pub struct DiagnosticWriter {
    diagnostics: Diagnostics,
    stream: DiagnosticStream,
}

impl DiagnosticWriter {
    pub fn new(stream: DiagnosticStream) -> Self {
        Self {
            diagnostics: Diagnostics::current(),
            stream,
        }
    }
}

impl io::Write for DiagnosticWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.diagnostics.write(self.stream, bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.diagnostics.flush(self.stream)
    }
}

#[macro_export]
macro_rules! diagnostic {
    ($stream:ident, $newline:expr, $($arg:tt)*) => {
        $crate::diagnostics::emit($crate::diagnostics::DiagnosticStream::$stream,
            format_args!($($arg)*), $newline)
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, Mutex};

    type Records = Vec<(DiagnosticStream, Vec<u8>)>;
    #[derive(Clone, Default)]
    struct Recording(Arc<Mutex<Records>>);
    impl DiagnosticSink for Recording {
        fn write(&self, stream: DiagnosticStream, bytes: &[u8]) -> io::Result<()> {
            self.0
                .lock()
                .expect("recording lock")
                .push((stream, bytes.to_vec()));
            Ok(())
        }
    }

    #[test]
    fn concurrent_nested_and_inherited_sinks_preserve_bytes_and_restore_scope() {
        let barrier = Barrier::new(2);
        std::thread::scope(|threads| {
            for name in ["left", "right"] {
                let barrier = &barrier;
                threads.spawn(move || {
                    let outer = Recording::default();
                    let inner = Recording::default();
                    Diagnostics::new(outer.clone()).scope(|| {
                        barrier.wait();
                        crate::diagnostic!(Stderr, true, "{name}");
                        Diagnostics::new(inner.clone()).scope(|| {
                            crate::diagnostic!(Stdout, false, "nested");
                        });
                        threads
                            .spawn(inherit(|| crate::diagnostic!(Stdout, true, "worker")))
                            .join()
                            .expect("worker");
                        let _ = std::panic::catch_unwind(|| {
                            Diagnostics::default().scope(|| panic!("test unwind"));
                        });
                        crate::diagnostic!(Stderr, false, "restored");
                    });
                    crate::diagnostic!(Stderr, true, "outside scope is silent");
                    assert_eq!(
                        *outer.0.lock().expect("outer lock"),
                        vec![
                            (DiagnosticStream::Stderr, format!("{name}\n").into_bytes()),
                            (DiagnosticStream::Stdout, b"worker\n".to_vec()),
                            (DiagnosticStream::Stderr, b"restored".to_vec()),
                        ]
                    );
                    assert_eq!(
                        *inner.0.lock().expect("inner lock"),
                        vec![(DiagnosticStream::Stdout, b"nested".to_vec()),]
                    );
                });
            }
        });
    }
}
