use std::sync::{Arc, Barrier, Mutex};

use sley_core::diagnostics::{DiagnosticSink, DiagnosticStream, Diagnostics};
use sley_core::{CallbackError, GitError, RejectionKind};

#[derive(Clone, Default)]
struct Recording(Arc<Mutex<Vec<u8>>>);

impl DiagnosticSink for Recording {
    fn write(&self, stream: DiagnosticStream, bytes: &[u8]) -> std::io::Result<()> {
        assert_eq!(stream, DiagnosticStream::Stderr);
        self.0.lock().expect("sink lock").extend_from_slice(bytes);
        Ok(())
    }
}

#[test]
fn library_config_errors_reach_only_the_callers_sink() {
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        for name in ["left.setting", "right.setting"] {
            let barrier = &barrier;
            scope.spawn(move || {
                let recording = Recording::default();
                let error = sley_config::typed::BadBooleanValue {
                    name: name.into(),
                    value: "invalid".into(),
                };
                Diagnostics::new(recording.clone()).scope(|| {
                    barrier.wait();
                    assert_eq!(error.report(), GitError::Rejected(RejectionKind::Refused));
                });
                // A default library call cannot keep writing into an earlier scope.
                assert_eq!(error.report(), GitError::Rejected(RejectionKind::Refused));
                assert_eq!(
                    *recording.0.lock().expect("sink lock"),
                    format!("fatal: bad boolean config value 'invalid' for '{name}'\n").as_bytes()
                );
            });
        }
    });
}

#[test]
fn callback_errors_preserve_the_concrete_source_and_io_keeps_its_kind() {
    use std::error::Error;
    let callback = CallbackError::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    assert_eq!(
        callback
            .downcast_ref::<std::io::Error>()
            .expect("concrete error")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    let error = GitError::Callback(callback);
    assert!(
        error
            .source()
            .expect("callback source")
            .source()
            .expect("original source")
            .is::<std::io::Error>()
    );
    assert_eq!(error, error.clone());
    assert_eq!(
        GitError::from(std::io::Error::from(std::io::ErrorKind::NotFound)).io_kind(),
        Some(std::io::ErrorKind::NotFound)
    );
}
