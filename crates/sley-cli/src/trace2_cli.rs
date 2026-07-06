use sley::{GitConfig, Result};
use crate::sley_config::{ConfigEntry, ConfigSection};
use std::env;
use std::sync::Mutex;

use crate::commands::remote::repo_current_branch_name;
use crate::global_options::injected_config_parameters;
use crate::repo_paths::common_git_dir_for_git_dir;
use crate::session;
use crate::sley_core;

static TRACE2_DEF_PARAMS_EMITTED: Mutex<bool> = Mutex::new(false);

fn trace2_target_enabled() -> bool {
    env::var_os("GIT_TRACE2").is_some()
        || env::var_os("GIT_TRACE2_PERF").is_some()
        || env::var_os("GIT_TRACE2_EVENT").is_some()
}

pub(crate) fn trace2_emit_process_ancestry_at_depth(depth: usize, prefix: &[&str]) {
    if !trace2_target_enabled() {
        return;
    }
    let parent_names = sley_procinfo::process_ancestry();
    if prefix.is_empty() && parent_names.is_empty() {
        return;
    }
    let mut ancestry = Vec::with_capacity(prefix.len() + parent_names.len());
    ancestry.extend(prefix.iter().map(|name| (*name).to_string()));
    ancestry.extend(parent_names.iter().cloned());
    sley_core::trace2::cmd_ancestry_at_depth(depth, &ancestry);
}

fn trace2_config_param_matches(pattern: &str, key: &str) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    let key = key.to_ascii_lowercase();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" || pattern == key {
        return true;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return false;
    }
    let mut remainder = key.as_str();
    if let Some(first) = parts.first().filter(|part| !part.is_empty()) {
        let Some(stripped) = remainder.strip_prefix(first) else {
            return false;
        };
        remainder = stripped;
    }
    for part in parts
        .iter()
        .skip(1)
        .take(parts.len().saturating_sub(2))
        .filter(|part| !part.is_empty())
    {
        let Some(idx) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[idx + part.len()..];
    }
    if let Some(last) = parts.last().filter(|part| !part.is_empty()) {
        return remainder.ends_with(last);
    }
    true
}

fn trace2_dispatch_config() -> Result<GitConfig> {
    let cwd = env::current_dir()?;
    let git_dir = session::cli_git_dir_from(&cwd).ok();
    let common_git_dir = git_dir
        .as_deref()
        .and_then(|git_dir| common_git_dir_for_git_dir(git_dir).ok());
    let branch = git_dir
        .as_deref()
        .and_then(|git_dir| repo_current_branch_name(git_dir));
    let context = crate::sley_config::ConfigIncludeContext::new(common_git_dir.clone(), branch);
    let mut config =
        crate::sley_config::load_pre_dispatch_config(common_git_dir.as_deref(), &context)?;
    let parameters = injected_config_parameters()?;
    crate::sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        &cwd,
    )?;
    Ok(config)
}

fn trace2_config_key(section: &ConfigSection, entry: &ConfigEntry) -> String {
    match section.subsection.as_deref() {
        Some(subsection) if !subsection.is_empty() => {
            format!("{}.{}.{}", section.name, subsection, entry.key)
        }
        _ => format!("{}.{}", section.name, entry.key),
    }
}

fn trace2_requested_config_patterns(config: &GitConfig) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(value) = env::var("GIT_TRACE2_CONFIG_PARAMS")
        && !value.is_empty()
    {
        out.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string),
        );
    }
    if let Some(value) = config
        .get("trace2", None, "configParams")
        .filter(|value| !value.is_empty())
    {
        out.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string),
        );
    }
    out
}

pub(crate) fn trace2_emit_def_params_at_depth(depth: usize) {
    if !trace2_target_enabled() {
        return;
    }
    let Ok(config) = trace2_dispatch_config() else {
        return;
    };
    let patterns = trace2_requested_config_patterns(&config);
    if !patterns.is_empty() {
        for section in &config.sections {
            for entry in &section.entries {
                let key = trace2_config_key(section, entry);
                if patterns
                    .iter()
                    .any(|pattern| trace2_config_param_matches(pattern, &key))
                {
                    let value = entry.value.as_deref().unwrap_or("true");
                    sley_core::trace2::def_param_at_depth(depth, &key, value);
                }
            }
        }
    }
    let envvars = env::var("GIT_TRACE2_ENV_VARS")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            config
                .get("trace2", None, "envvars")
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    if let Some(envvars) = envvars {
        for name in envvars
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            if let Ok(value) = env::var(name) {
                sley_core::trace2::def_param_at_depth(depth, name, value);
            }
        }
    }
}

pub(crate) fn trace2_emit_def_params_once() {
    let mut emitted = TRACE2_DEF_PARAMS_EMITTED.lock().unwrap();
    if *emitted {
        return;
    }
    *emitted = true;
    drop(emitted);
    trace2_emit_def_params_at_depth(sley_core::trace2::depth());
}

pub(crate) fn trace_reference_fsync_counter(count: usize) {
    if count == 0 || env::var_os("GIT_TRACE2_EVENT").is_none() {
        return;
    }
    if !env::var("GIT_TEST_FSYNC").is_ok_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    }) {
        return;
    }
    let Ok(Some(components)) = crate::global_config_value("core.fsync") else {
        return;
    };
    let references_are_synced = components
        .split([',', ' ', '\t'])
        .filter(|component| !component.is_empty())
        .any(|component| {
            component.eq_ignore_ascii_case("reference") || component.eq_ignore_ascii_case("all")
        });
    if !references_are_synced {
        return;
    }
    sley_core::trace2::counter("fsync", "hardware-flush", count);
}