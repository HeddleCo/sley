//! Typed submodule administration plans for embedders.

use sley_config::{ConfigEntry, GitConfig};
use std::fmt;

/// One `.gitmodules` entry selected for `submodule init`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitCandidate {
    pub name: String,
    pub path: String,
    pub url: Option<String>,
    pub update: Option<String>,
    pub currently_active: bool,
}

/// A repository-config change produced by a submodule administration plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigMutation {
    Set {
        name: String,
        key: String,
        value: String,
    },
}

/// A newly registered submodule URL, retained for porcelain rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitRegistration {
    pub name: String,
    pub path: String,
    pub url: String,
}

/// The deterministic configuration and rendering outcome of `submodule init`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitPlan {
    pub mutations: Vec<ConfigMutation>,
    pub registrations: Vec<InitRegistration>,
}

impl InitPlan {
    /// Whether applying this plan changes repository configuration.
    pub fn changed(&self) -> bool {
        !self.mutations.is_empty()
    }
}

/// A semantic failure while planning `submodule init`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitPlanError {
    MissingUrl { path: String },
}

impl fmt::Display for InitPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingUrl { path } => write!(
                formatter,
                "no URL found for submodule path '{path}' in .gitmodules"
            ),
        }
    }
}

impl std::error::Error for InitPlanError {}

/// Plan `submodule init` configuration without performing filesystem I/O.
///
/// `resolve_url` is injected because relative URL resolution depends on the
/// embedding application's effective superproject remote configuration.
pub fn plan_init<F>(
    config: &GitConfig,
    candidates: &[InitCandidate],
    mut resolve_url: F,
) -> Result<InitPlan, InitPlanError>
where
    F: FnMut(&str) -> String,
{
    let mut shadow = config.clone();
    let mut plan = InitPlan::default();
    for candidate in candidates {
        if !candidate.currently_active {
            push_set(&mut shadow, &mut plan, &candidate.name, "active", "true");
        }
        if shadow
            .get("submodule", Some(&candidate.name), "url")
            .is_none()
        {
            let Some(url) = candidate.url.as_deref() else {
                return Err(InitPlanError::MissingUrl {
                    path: candidate.path.clone(),
                });
            };
            let url = resolve_url(url);
            push_set(&mut shadow, &mut plan, &candidate.name, "url", &url);
            plan.registrations.push(InitRegistration {
                name: candidate.name.clone(),
                path: candidate.path.clone(),
                url,
            });
        }
        if shadow
            .get("submodule", Some(&candidate.name), "update")
            .is_none()
            && let Some(update) = candidate.update.as_deref()
        {
            push_set(&mut shadow, &mut plan, &candidate.name, "update", update);
        }
    }
    Ok(plan)
}

fn push_set(shadow: &mut GitConfig, plan: &mut InitPlan, name: &str, key: &str, value: &str) {
    set_submodule_value(shadow, name, key, value);
    plan.mutations.push(ConfigMutation::Set {
        name: name.to_string(),
        key: key.to_string(),
        value: value.to_string(),
    });
}

/// Apply a previously validated init plan to an in-memory repository config.
pub fn apply_init_plan(config: &mut GitConfig, plan: &InitPlan) {
    for mutation in &plan.mutations {
        match mutation {
            ConfigMutation::Set { name, key, value } => {
                set_submodule_value(config, name, key, value);
            }
        }
    }
}

fn set_submodule_value(config: &mut GitConfig, name: &str, key: &str, value: &str) {
    if let Some(section) =
        config.sections.iter_mut().rev().find(|section| {
            section.name == "submodule" && section.subsection.as_deref() == Some(name)
        })
    {
        if let Some(entry) = section
            .entries
            .iter_mut()
            .find(|entry| entry.key.eq_ignore_ascii_case(key))
        {
            entry.value = Some(value.to_string());
        } else {
            section
                .entries
                .push(ConfigEntry::new(key, Some(value.to_string())));
        }
    } else {
        config.sections.push(sley_config::ConfigSection::new(
            "submodule",
            Some(name.to_string()),
            vec![ConfigEntry::new(key, Some(value.to_string()))],
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_plan_keeps_independent_active_url_and_update_guards() {
        let config =
            GitConfig::parse(b"[submodule \"module\"]\n\turl = existing\n").expect("parse config");
        let plan = plan_init(
            &config,
            &[InitCandidate {
                name: "module".into(),
                path: "libs/module".into(),
                url: Some("ignored".into()),
                update: Some("merge".into()),
                currently_active: false,
            }],
            str::to_string,
        )
        .expect("plan init");
        assert_eq!(plan.registrations, Vec::new());
        assert_eq!(plan.mutations.len(), 2);
        let mut applied = config;
        apply_init_plan(&mut applied, &plan);
        assert_eq!(
            applied.get("submodule", Some("module"), "active"),
            Some("true")
        );
        assert_eq!(
            applied.get("submodule", Some("module"), "update"),
            Some("merge")
        );
    }

    #[test]
    fn init_plan_returns_typed_missing_url() {
        let error = plan_init(
            &GitConfig::default(),
            &[InitCandidate {
                name: "module".into(),
                path: "module".into(),
                url: None,
                update: None,
                currently_active: false,
            }],
            str::to_string,
        )
        .expect_err("missing URL");
        assert_eq!(
            error,
            InitPlanError::MissingUrl {
                path: "module".into()
            }
        );
    }

    #[test]
    fn init_plan_resolves_and_reports_new_registration() {
        let config = GitConfig::default();
        let plan = plan_init(
            &config,
            &[InitCandidate {
                name: "module".into(),
                path: "libs/module".into(),
                url: Some("../module".into()),
                update: None,
                currently_active: true,
            }],
            |url| format!("https://example.test/{url}"),
        )
        .expect("plan init");
        assert_eq!(
            plan.registrations,
            vec![InitRegistration {
                name: "module".into(),
                path: "libs/module".into(),
                url: "https://example.test/../module".into(),
            }]
        );
        assert_eq!(
            config.get("submodule", Some("module"), "url"),
            None,
            "planning must not mutate the caller's config"
        );
        let mut applied = config;
        apply_init_plan(&mut applied, &plan);
        assert_eq!(
            applied.get("submodule", Some("module"), "url"),
            Some("https://example.test/../module")
        );
    }
}
