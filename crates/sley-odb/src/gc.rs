//! Typed garbage-collection repack planning.

use std::error::Error;
use std::fmt;

/// Repack strategy selected for one GC run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcRepackMode {
    /// Pack loose objects only (`repack -d -l --no-write-bitmap-index`).
    Incremental,
    /// Repack reachable objects and immediately discard unreachable data.
    Immediate,
    /// Write reachable and cruft packs.
    Cruft,
    /// Repack reachable objects and unpack/prune unreachable data separately.
    Reachable,
}

/// Inputs which determine GC's repack strategy and child-command trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcRepackPlanOptions<'a> {
    pub incremental: bool,
    pub prune_expire: Option<&'a str>,
    pub cruft_packs: bool,
    pub expire_to: Option<&'a str>,
    pub max_cruft_size: Option<u64>,
    pub repack_filter: Option<&'a str>,
    pub repack_filter_to: Option<&'a str>,
}

/// Planned engine strategy and Git-compatible repack argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcRepackPlanOutcome {
    pub mode: GcRepackMode,
    pub trace_args: Vec<String>,
}

/// Invalid GC planning input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcRepackPlanError {
    /// An explicitly supplied prune expiration was empty.
    EmptyPruneExpiration,
}

impl fmt::Display for GcRepackPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPruneExpiration => formatter.write_str("empty GC prune expiration"),
        }
    }
}

impl Error for GcRepackPlanError {}

/// Select GC's repack family without touching repository or process state.
pub fn plan_gc_repack(
    options: GcRepackPlanOptions<'_>,
) -> Result<GcRepackPlanOutcome, GcRepackPlanError> {
    if options.prune_expire.is_some_and(str::is_empty) {
        return Err(GcRepackPlanError::EmptyPruneExpiration);
    }
    if options.incremental {
        return Ok(GcRepackPlanOutcome {
            mode: GcRepackMode::Incremental,
            trace_args: strings(&["repack", "-d", "-l", "--no-write-bitmap-index"]),
        });
    }
    if options.prune_expire == Some("now") && !(options.cruft_packs && options.expire_to.is_some())
    {
        return Ok(GcRepackPlanOutcome {
            mode: GcRepackMode::Immediate,
            trace_args: strings(&["repack", "-d", "-l", "-a"]),
        });
    }

    let mut trace_args = strings(if options.cruft_packs {
        &["repack", "-d", "-l", "--cruft"]
    } else {
        &["repack", "-d", "-l", "-A"]
    });
    if options.cruft_packs {
        if let Some(expiration) = options.prune_expire {
            trace_args.push(format!("--cruft-expiration={expiration}"));
        }
        if let Some(size) = options.max_cruft_size {
            trace_args.push(format!("--max-cruft-size={size}"));
        }
        if let Some(expire_to) = options.expire_to {
            trace_args.push(format!("--expire-to={expire_to}"));
        }
    } else if let Some(expiration) = options.prune_expire {
        trace_args.push(format!("--unpack-unreachable={expiration}"));
    }
    if let Some(filter) = options.repack_filter {
        trace_args.push(format!("--filter={filter}"));
    }
    if let Some(filter_to) = options.repack_filter_to {
        trace_args.push(format!("--filter-to={filter_to}"));
    }
    Ok(GcRepackPlanOutcome {
        mode: if options.cruft_packs {
            GcRepackMode::Cruft
        } else {
            GcRepackMode::Reachable
        },
        trace_args,
    })
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> GcRepackPlanOptions<'static> {
        GcRepackPlanOptions {
            incremental: false,
            prune_expire: Some("2.weeks.ago"),
            cruft_packs: true,
            expire_to: None,
            max_cruft_size: None,
            repack_filter: None,
            repack_filter_to: None,
        }
    }

    #[test]
    fn incremental_and_immediate_modes_take_precedence() {
        let incremental = plan_gc_repack(GcRepackPlanOptions {
            incremental: true,
            ..options()
        })
        .expect("incremental plan should succeed");
        assert_eq!(incremental.mode, GcRepackMode::Incremental);

        let immediate = plan_gc_repack(GcRepackPlanOptions {
            prune_expire: Some("now"),
            cruft_packs: false,
            ..options()
        })
        .expect("immediate plan should succeed");
        assert_eq!(immediate.mode, GcRepackMode::Immediate);
        assert_eq!(immediate.trace_args, strings(&["repack", "-d", "-l", "-a"]));
    }

    #[test]
    fn cruft_plan_preserves_every_engine_option_in_order() {
        let plan = plan_gc_repack(GcRepackPlanOptions {
            expire_to: Some("archive"),
            max_cruft_size: Some(42),
            repack_filter: Some("blob:none"),
            repack_filter_to: Some("filtered"),
            ..options()
        })
        .expect("cruft plan should succeed");
        assert_eq!(plan.mode, GcRepackMode::Cruft);
        assert_eq!(
            plan.trace_args,
            strings(&[
                "repack",
                "-d",
                "-l",
                "--cruft",
                "--cruft-expiration=2.weeks.ago",
                "--max-cruft-size=42",
                "--expire-to=archive",
                "--filter=blob:none",
                "--filter-to=filtered",
            ])
        );
    }

    #[test]
    fn reachable_plan_uses_unpack_expiration() {
        let plan = plan_gc_repack(GcRepackPlanOptions {
            cruft_packs: false,
            ..options()
        })
        .expect("reachable plan should succeed");
        assert_eq!(plan.mode, GcRepackMode::Reachable);
        assert_eq!(
            plan.trace_args,
            strings(&[
                "repack",
                "-d",
                "-l",
                "-A",
                "--unpack-unreachable=2.weeks.ago"
            ])
        );
    }
}
