//! Integration test that runs UPSTREAM git's own `t/*.sh` suite against the
//! sley binary (the ultimate parity oracle).
//!
//! GATING: this test is `#[ignore]` by default so a plain `cargo test
//! --workspace` never runs it, and it *also* skips cleanly (prints a message
//! and returns `Ok`) when no upstream `t/` directory is configured. So even if
//! someone runs it explicitly in an unconfigured environment, it passes.
//!
//! To actually exercise upstream parity:
//!
//! ```sh
//! # 1. Get a built git source checkout (see scripts/run-upstream-tests.sh):
//! git clone --depth=1 https://github.com/git/git /tmp/git-src
//! cd /tmp/git-src && make GIT-BUILD-OPTIONS t/helper/test-tool && \
//!     ( cd templates && make )
//!
//! # 2. Run this test against it:
//! GIT_SRC_DIR=/tmp/git-src \
//!     cargo test -p sley-testkit --test upstream -- --ignored --nocapture
//!
//! Set `SLEY_UPSTREAM_REQUIRE_PASS=1` to turn the measured parity run into a
//! gating assertion that every selected upstream script passes.
//! ```

use sley_testkit::upstream::{self, UpstreamRunOutcome};

#[test]
#[ignore = "runs upstream git's t/*.sh suite; requires GIT_SRC_DIR or SLEY_UPSTREAM_T pointing at a built git checkout"]
fn upstream_default_subset_runs() {
    // `CARGO_BIN_EXE_sley` is injected by Cargo when this crate has the
    // git-cli binary in scope; fall back to letting the runner resolve it.
    let bin = option_env!("CARGO_BIN_EXE_sley").map(std::path::Path::new);

    let outcome = match upstream::run_upstream_default_subset(bin) {
        Ok(outcome) => outcome,
        Err(err) => panic!("upstream runner failed to launch: {err}"),
    };

    match outcome {
        UpstreamRunOutcome::Skipped(reason) => {
            // Clean skip: nothing configured. Still a pass.
            eprintln!("SKIP upstream parity: {reason}");
        }
        UpstreamRunOutcome::Ran {
            results,
            report_path,
            all_passed,
        } => {
            eprintln!(
                "Upstream parity results (full report: {}):",
                report_path.display()
            );
            for r in &results {
                eprintln!(
                    "  {:<28} {:<8} ok={:<5} not-ok={}",
                    r.script, r.result, r.ok, r.failed
                );
            }
            assert!(
                !results.is_empty(),
                "runner produced no parseable per-script results"
            );
            if all_passed {
                eprintln!("All selected upstream scripts passed.");
            } else if std::env::var_os("SLEY_UPSTREAM_REQUIRE_PASS").is_some()
                || std::env::var_os("GIT_RS_UPSTREAM_REQUIRE_PASS").is_some()
            {
                panic!("some selected upstream scripts failed; see the report above");
            } else {
                eprintln!(
                    "Some upstream scripts did not fully pass (expected during \
                     development); see the report above for details."
                );
            }
        }
    }
}
