//! Process-level cooperative cancel driven by Ctrl-C / SIGINT.
//!
//! Uses the `ctrlc` crate (signal handling lives outside this crate's
//! `forbid(unsafe_code)` boundary). The first call installs a process-wide
//! handler that trips a shared [`AtomicCancel`]; later calls reuse it.
//!
//! Fetch/clone/push pass the flag into `sley_remote` services so pack
//! receive/generate can stop cooperatively with [`GitError::Cancelled`]
//! (CLI exit 130) instead of only dying mid-syscall.
//!
//! A **second** Ctrl-C while already cancelled exits immediately (130), so a
//! hung non-polling phase is still escapable without SIGKILL.

use sley::{AtomicCancel, CancelFlag};
use std::sync::{Arc, OnceLock};

static PROCESS_CANCEL: OnceLock<Arc<AtomicCancel>> = OnceLock::new();

/// Shared process cancel source. Installs a Ctrl-C handler on first use.
///
/// Callers should keep the returned [`Arc`] alive for the duration of the
/// operation and build a [`CancelFlag`] via [`dyn_cancel_flag`].
pub(crate) fn process_interrupt_cancel() -> Arc<AtomicCancel> {
    PROCESS_CANCEL
        .get_or_init(|| {
            let flag = Arc::new(AtomicCancel::new());
            let for_handler = Arc::clone(&flag);
            // Best-effort: if a handler is already installed (tests / embedders),
            // the AtomicCancel is still usable when set from other code.
            let _ = ctrlc::set_handler(move || {
                if for_handler.is_cancelled() {
                    // Second interrupt: escape immediately (git dies on SIGINT
                    // anywhere; cooperative polling cannot cover every phase).
                    std::process::exit(130);
                }
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

/// Cancel handle for `FetchServices` / `CloneServices` / `PushServices`.
pub(crate) fn dyn_cancel_flag(flag: &AtomicCancel) -> CancelFlag<'_> {
    CancelFlag::new(flag)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn sigint_cancels_then_exits_130() {
        const CHILD: &str = "SLEY_INTERRUPT_DEPENDENCY_TEST";
        if std::env::var_os(CHILD).is_some() {
            let flag = process_interrupt_cancel();
            let mut output = std::io::stdout().lock();
            writeln!(output, "handler ready").expect("announce handler");
            output.flush().expect("flush readiness");
            let deadline = Instant::now() + Duration::from_secs(5);
            while !flag.is_cancelled() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(
                dyn_cancel_flag(&flag).is_cancelled(),
                "first SIGINT must cancel"
            );
            writeln!(output, "cancelled").expect("announce cancellation");
            output.flush().expect("flush cancellation");
            std::thread::sleep(Duration::from_secs(5));
            panic!("second SIGINT must exit immediately");
        }
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "interrupt_cancel::tests::sigint_cancels_then_exits_130",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(Stdio::piped())
            .spawn()
            .expect("start isolated signal test");
        let mut output = BufReader::new(child.stdout.take().expect("child stdout"));
        for expected in ["handler ready", "cancelled"] {
            let mut line = String::new();
            loop {
                assert_ne!(
                    output.read_line(&mut line).expect("read child status"),
                    0,
                    "child exited before {expected}"
                );
                if line.trim() == expected {
                    break;
                }
                line.clear();
            }
            assert!(
                Command::new("kill")
                    .args(["-INT", &child.id().to_string()])
                    .status()
                    .expect("send SIGINT")
                    .success()
            );
        }
        assert_eq!(
            child.wait().expect("wait for interrupted process").code(),
            Some(130)
        );
    }
}
