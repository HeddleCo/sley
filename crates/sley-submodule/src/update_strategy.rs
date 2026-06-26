//! Submodule update-strategy resolution — a Rust port of git's
//! `builtin/submodule--helper.c::determine_submodule_update_strategy` plus the
//! per-strategy execution dispatch (`run_update_command`).
//!
//! `submodule update` must, for each selected submodule, pick exactly ONE update
//! mode and run it against the gitlink oid recorded in the superproject index.
//! git resolves the mode from a fixed precedence:
//!
//! 1. the command-line `--checkout`/`--merge`/`--rebase` flag (the `update`
//!    default passed into `update_submodule`), if any;
//! 2. else `.git/config submodule.<name>.update`;
//! 3. else the `.gitmodules submodule.<name>.update` value parsed into the typed
//!    [`UpdateStrategy`] (but NOT a `!command` — git BUG()s if it ever reads a
//!    command out of `.gitmodules`, because `init` refuses to copy one);
//! 4. else [`UpdateType::Checkout`].
//!
//! Then a `just_cloned` submodule downgrades `merge`/`rebase`/`none` to
//! `checkout` (there is no local HEAD to merge/rebase onto yet).
//!
//! Centralizing this here means the CLI's `submodule update` routes every mode
//! through one resolver + one dispatch, so the whole update CLASS (checkout,
//! merge, rebase, command, none-skip) converges on a single code path instead of
//! the old checkout-only special case.

use crate::config::{UpdateStrategy, UpdateType, parse_update_strategy};

/// Port of `determine_submodule_update_strategy`. Resolves the effective update
/// strategy for one submodule from the precedence above.
///
/// - `cli_default` — the strategy forced by a `--checkout`/`--merge`/`--rebase`
///   flag (`UpdateType::Unspecified` when none was given).
/// - `config_update` — the raw `submodule.<name>.update` value from
///   `.git/config` (None when unset).
/// - `gitmodules_strategy` — the typed strategy parsed from `.gitmodules`.
/// - `just_cloned` — true when the submodule worktree was populated by this very
///   `update` run (no pre-existing HEAD to merge/rebase onto).
///
/// Returns `Err(invalid_value)` when the `.git/config` value is present but
/// unparseable, mirroring git's `die("Invalid update mode '%s' …")`.
pub fn determine_update_strategy(
    cli_default: UpdateType,
    config_update: Option<&str>,
    gitmodules_strategy: &UpdateStrategy,
    just_cloned: bool,
) -> Result<UpdateStrategy, String> {
    let mut out = if cli_default != UpdateType::Unspecified {
        UpdateStrategy {
            kind: cli_default,
            command: None,
        }
    } else if let Some(val) = config_update {
        // git: parse_submodule_update_strategy(val, out) < 0 -> die "Invalid …".
        match parse_update_strategy(val) {
            Some(strategy) => strategy,
            None => return Err(val.to_string()),
        }
    } else if gitmodules_strategy.kind != UpdateType::Unspecified {
        // git BUG()s if .gitmodules ever yields a Command; init refuses to copy
        // one, so by construction we never see it here. Carry the typed value
        // through (command stays None for the non-Command kinds).
        gitmodules_strategy.clone()
    } else {
        UpdateStrategy {
            kind: UpdateType::Checkout,
            command: None,
        }
    };

    if just_cloned
        && matches!(
            out.kind,
            UpdateType::Merge | UpdateType::Rebase | UpdateType::None
        )
    {
        out.kind = UpdateType::Checkout;
        out.command = None;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout() -> UpdateStrategy {
        UpdateStrategy {
            kind: UpdateType::Checkout,
            command: None,
        }
    }

    fn unspec() -> UpdateStrategy {
        UpdateStrategy::default()
    }

    #[test]
    fn cli_flag_wins() {
        // --rebase given: beats .git/config merge and .gitmodules none.
        let got = determine_update_strategy(
            UpdateType::Rebase,
            Some("merge"),
            &UpdateStrategy {
                kind: UpdateType::None,
                command: None,
            },
            false,
        )
        .expect("CLI update strategy should be accepted");
        assert_eq!(got.kind, UpdateType::Rebase);
    }

    #[test]
    fn config_beats_gitmodules() {
        let got = determine_update_strategy(
            UpdateType::Unspecified,
            Some("rebase"),
            &UpdateStrategy {
                kind: UpdateType::Merge,
                command: None,
            },
            false,
        )
        .expect("configured update strategy should be accepted");
        assert_eq!(got.kind, UpdateType::Rebase);
    }

    #[test]
    fn gitmodules_used_when_no_config() {
        let got = determine_update_strategy(
            UpdateType::Unspecified,
            None,
            &UpdateStrategy {
                kind: UpdateType::Merge,
                command: None,
            },
            false,
        )
        .expect("gitmodules update strategy should be accepted");
        assert_eq!(got.kind, UpdateType::Merge);
    }

    #[test]
    fn default_is_checkout() {
        let got = determine_update_strategy(UpdateType::Unspecified, None, &unspec(), false)
            .expect("default update strategy should be accepted");
        assert_eq!(got, checkout());
    }

    #[test]
    fn command_from_config_carries_string() {
        let got =
            determine_update_strategy(UpdateType::Unspecified, Some("!false"), &unspec(), false)
                .expect("custom update command should be accepted");
        assert_eq!(got.kind, UpdateType::Command);
        assert_eq!(got.command.as_deref(), Some("false"));
    }

    #[test]
    fn just_cloned_downgrades_to_checkout() {
        for kind in [UpdateType::Merge, UpdateType::Rebase, UpdateType::None] {
            let got = determine_update_strategy(
                UpdateType::Unspecified,
                None,
                &UpdateStrategy {
                    kind,
                    command: None,
                },
                true,
            )
            .expect("just-cloned strategy should be accepted");
            assert_eq!(got.kind, UpdateType::Checkout, "downgrade {kind:?}");
        }
        // checkout / command are NOT downgraded.
        let got =
            determine_update_strategy(UpdateType::Unspecified, Some("!true"), &unspec(), true)
                .expect("custom update command should not be downgraded");
        assert_eq!(got.kind, UpdateType::Command);
    }

    #[test]
    fn invalid_config_value_errors() {
        let err =
            determine_update_strategy(UpdateType::Unspecified, Some("bogus"), &unspec(), false)
                .expect_err("invalid update strategy should error");
        assert_eq!(err, "bogus");
    }
}
