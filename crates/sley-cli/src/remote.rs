//! Remote URL resolution and transport helpers shared by fetch/push/ls-remote.
//!
//! Delegates to [`sley_config::remotes`] and [`sley_remote::resolve`] so the CLI
//! does not maintain parallel `insteadOf` / `pushurl` logic.

pub use sley_config::remotes::{
    remote_config_values, resolve_remote_fetch_url, resolve_remote_push_url,
    rewrite_url_with_config,
};
