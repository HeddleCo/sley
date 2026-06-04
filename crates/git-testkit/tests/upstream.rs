//! Integration test that runs UPSTREAM git's own `t/*.sh` suite against the
//! git-rs binary (the ultimate parity oracle).
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
//!     cargo test -p git-testkit --test upstream -- --ignored --nocapture
//! ```

use git_testkit::upstream::{self, UpstreamRunOutcome};

#[test]
#[ignore = "runs upstream git's t/*.sh suite; requires GIT_SRC_DIR or GIT_RS_UPSTREAM_T pointing at a built git checkout"]
fn upstream_default_subset_runs() {
    // `CARGO_BIN_EXE_git-rs` is injected by Cargo when this crate has the
    // git-cli binary in scope; fall back to letting the runner resolve it.
    let bin = option_env!("CARGO_BIN_EXE_git-rs").map(std::path::Path::new);

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
            // We do NOT assert `all_passed`: git-rs is not yet at full upstream
            // parity, so this test measures and reports the gap rather than
            // gating on a green suite. Flip this to an assertion once parity is
            // expected.
            if all_passed {
                eprintln!("All selected upstream scripts passed.");
            } else {
                eprintln!(
                    "Some upstream scripts did not fully pass (expected during \
                     development); see the report above for details."
                );
            }
        }
    }
}
