//! Source-aware config snapshots and byte-preserving edit plans.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use sley_config::raw_edit::{
    ConfigFileWriteError, ConfigFileWriteOptions, RawConfigEditor, RawEditOutcome,
    SectionEditOutcome, rename_or_remove_section, write_config_file_locked,
};
use sley_config::{ConfigOriginKind, ConfigScope, ConfigStack};

use crate::{GitError, Repository};

/// One config value with the physical source it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValue {
    /// Canonical `section[.subsection].name`.
    pub key: String,
    /// Parsed value. `None` represents a bare boolean-true key.
    pub value: Option<String>,
    /// Scope/source metadata suitable for deciding whether the value can be edited.
    pub source: ConfigSource,
}

/// A flattened effective config stream, lowest precedence first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigSnapshot {
    pub values: Vec<ConfigValue>,
}

impl ConfigSnapshot {
    /// Highest-precedence value for `key`.
    pub fn get(&self, key: &str) -> Result<Option<&ConfigValue>, ConfigEditError> {
        let key = ParsedConfigKey::parse(key)?;
        Ok(self.values.iter().rev().find(|value| value.key == key.full))
    }

    /// All values for `key`, in precedence order.
    pub fn get_all(&self, key: &str) -> Result<Vec<&ConfigValue>, ConfigEditError> {
        let key = ParsedConfigKey::parse(key)?;
        Ok(self
            .values
            .iter()
            .filter(|value| value.key == key.full)
            .collect())
    }
}

/// Worktree config inclusion policy for [`ConfigStackOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeConfig {
    Never,
    Always,
    WhenEnabled,
}

/// Options for building an effective, source-attributed Git config stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigStackOptions {
    follow_includes: bool,
    worktree_config: WorktreeConfig,
    track_origins: bool,
}

impl ConfigStackOptions {
    /// Git's repository config behavior: follow includes and include worktree
    /// config only when `extensions.worktreeConfig` enables it.
    pub fn git_default() -> Self {
        Self {
            follow_includes: true,
            worktree_config: WorktreeConfig::WhenEnabled,
            track_origins: true,
        }
    }

    pub fn follow_includes(mut self, follow_includes: bool) -> Self {
        self.follow_includes = follow_includes;
        self
    }

    pub fn worktree_config(mut self, worktree_config: WorktreeConfig) -> Self {
        self.worktree_config = worktree_config;
        self
    }

    pub fn track_origins(mut self, track_origins: bool) -> Self {
        self.track_origins = track_origins;
        self
    }

    pub fn includes_followed(self) -> bool {
        self.follow_includes
    }

    pub fn worktree_config_policy(self) -> WorktreeConfig {
        self.worktree_config
    }

    pub fn origins_tracked(self) -> bool {
        self.track_origins
    }
}

impl Default for ConfigStackOptions {
    fn default() -> Self {
        Self::git_default()
    }
}

/// Stable id for a source-backed logical config section in a
/// [`ConfigStackView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConfigSectionId(usize);

/// A source-attributed effective config stack with helper lookups for remotes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigStackView {
    pub values: ConfigSnapshot,
    pub remotes: RemoteConfigSnapshot,
    section_sources: Vec<RemoteConfigSource>,
}

impl ConfigStackView {
    pub fn values(&self) -> &ConfigSnapshot {
        &self.values
    }

    pub fn remotes(&self) -> &RemoteConfigSnapshot {
        &self.remotes
    }

    /// Return a configured remote with the source section selected for editing.
    pub fn remote(&self, name: &str) -> Result<ConfigRemote, ConfigEditError> {
        let Some(remote) = self.remotes.get(name).cloned() else {
            return Err(ConfigEditError::SectionNotFound {
                section: "remote".to_string(),
                subsection: Some(name.to_string()),
            });
        };
        let Some(source) = remote.sources.last().cloned() else {
            return Err(ConfigEditError::NoEditableSource);
        };
        let section_id = self
            .section_sources
            .iter()
            .position(|candidate| candidate == &source)
            .ok_or(ConfigEditError::NoEditableSource)?;
        Ok(ConfigRemote {
            remote,
            section_id: ConfigSectionId(section_id),
        })
    }

    /// Physical file that can safely edit the section identified by `section_id`.
    pub fn editable_section_file(
        &self,
        section_id: ConfigSectionId,
    ) -> Result<PathBuf, ConfigEditError> {
        let source = self
            .section_sources
            .get(section_id.0)
            .ok_or(ConfigEditError::NoEditableSource)?;
        if source.editable
            && let Some(path) = &source.target_path
        {
            return Ok(path.clone());
        }
        match &source.refusal {
            Some(RemoteConfigRefusal::ExternalInclude { path }) => {
                Err(ConfigEditError::RefusesExternalInclude { path: path.clone() })
            }
            Some(RemoteConfigRefusal::SyntheticSource) | None => {
                Err(ConfigEditError::NoEditableSource)
            }
        }
    }
}

/// A remote returned from [`ConfigStackView::remote`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRemote {
    remote: RemoteConfig,
    section_id: ConfigSectionId,
}

impl ConfigRemote {
    pub fn section_id(&self) -> ConfigSectionId {
        self.section_id
    }

    pub fn remote(&self) -> &RemoteConfig {
        &self.remote
    }

    pub fn name(&self) -> &str {
        &self.remote.name
    }
}

/// Source-attributed remote configuration, lowest precedence first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteConfigSnapshot {
    pub remotes: Vec<RemoteConfig>,
}

impl RemoteConfigSnapshot {
    pub fn get(&self, name: &str) -> Option<&RemoteConfig> {
        self.remotes.iter().find(|remote| remote.name == name)
    }
}

/// One logical `[remote "<name>"]` assembled from the effective config stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    pub name: String,
    pub values: Vec<RemoteConfigValue>,
    pub sources: Vec<RemoteConfigSource>,
}

impl RemoteConfig {
    pub fn urls(&self) -> Vec<&str> {
        self.values_for("url")
    }

    pub fn push_urls(&self) -> Vec<&str> {
        self.values_for("pushurl")
    }

    pub fn fetch_refspecs(&self) -> Vec<&str> {
        self.values_for("fetch")
    }

    pub fn values_for(&self, name: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|value| value.name.eq_ignore_ascii_case(name))
            .filter_map(|value| value.value.as_deref())
            .collect()
    }
}

/// One key/value inside a remote section with its physical source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfigValue {
    pub name: String,
    pub value: Option<String>,
    pub source: ConfigSource,
}

/// A physical source contributing to a remote, plus editability metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfigSource {
    pub source: ConfigSource,
    pub editable: bool,
    pub target_path: Option<PathBuf>,
    pub refusal: Option<RemoteConfigRefusal>,
}

/// Why a remote source cannot be edited by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteConfigRefusal {
    ExternalInclude { path: PathBuf },
    SyntheticSource,
}

/// The physical or synthetic origin of a config value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    Local {
        path: PathBuf,
    },
    Worktree {
        path: PathBuf,
    },
    Global {
        path: PathBuf,
    },
    System {
        path: PathBuf,
    },
    Included {
        path: PathBuf,
        included_from: Option<PathBuf>,
    },
    Env,
    CommandLine,
    Blob {
        spec: String,
    },
    Stdin,
}

/// Scope/target selection for planning a config edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigEditScope {
    /// Edit `<common_git_dir>/config`.
    Local,
    /// Edit `<git_dir>/config.worktree`.
    Worktree,
    /// Edit the highest-precedence global config file discovered for this process.
    Global,
    /// Edit the system config file discovered for this process.
    System,
    /// Edit the highest-precedence existing value's physical source.
    ExistingValue { allow_external_includes: bool },
    /// Edit an explicit physical path.
    Path(PathBuf),
}

/// A byte-preserving edit plan for one physical config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEditPlan {
    pub target_path: PathBuf,
    pub operations: Vec<ConfigEdit>,
    pub fsync: bool,
}

impl ConfigEditPlan {
    pub fn new(target_path: impl Into<PathBuf>) -> Self {
        Self {
            target_path: target_path.into(),
            operations: Vec::new(),
            fsync: false,
        }
    }

    pub fn with_operation(mut self, operation: ConfigEdit) -> Self {
        self.operations.push(operation);
        self
    }

    pub fn with_fsync(mut self, fsync: bool) -> Self {
        self.fsync = fsync;
        self
    }
}

/// One key/value written into a config section operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSectionEntry {
    pub name: String,
    pub value: Option<String>,
}

impl ConfigSectionEntry {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: Some(value.into()),
        }
    }

    pub fn bare(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: None,
        }
    }
}

/// One edit within a [`ConfigEditPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigEdit {
    Set {
        section: String,
        subsection: Option<String>,
        name: String,
        value: String,
    },
    Unset {
        section: String,
        subsection: Option<String>,
        name: String,
    },
    ReplaceSection {
        section: String,
        subsection: Option<String>,
        entries: Vec<ConfigSectionEntry>,
    },
    RemoveSection {
        section: String,
        subsection: Option<String>,
    },
}

impl ConfigEdit {
    pub fn set(key: &str, value: impl Into<String>) -> Result<Self, ConfigEditError> {
        let key = ParsedConfigKey::parse(key)?;
        Ok(Self::Set {
            section: key.section,
            subsection: key.subsection,
            name: key.name,
            value: value.into(),
        })
    }

    pub fn unset(key: &str) -> Result<Self, ConfigEditError> {
        let key = ParsedConfigKey::parse(key)?;
        Ok(Self::Unset {
            section: key.section,
            subsection: key.subsection,
            name: key.name,
        })
    }

    pub fn replace_section(
        section: impl Into<String>,
        subsection: Option<String>,
        entries: Vec<ConfigSectionEntry>,
    ) -> Self {
        Self::ReplaceSection {
            section: section.into(),
            subsection,
            entries,
        }
    }

    pub fn remove_section(section: impl Into<String>, subsection: Option<String>) -> Self {
        Self::RemoveSection {
            section: section.into(),
            subsection,
        }
    }
}

/// A whole-section replacement for `[remote "<name>"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfigSet {
    pub name: String,
    pub entries: Vec<ConfigSectionEntry>,
}

impl RemoteConfigSet {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entries: Vec::new(),
        }
    }

    pub fn with_entry(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.entries.push(ConfigSectionEntry::new(name, value));
        self
    }

    pub fn with_url(self, url: impl Into<String>) -> Self {
        self.with_entry("url", url)
    }

    pub fn with_push_url(self, url: impl Into<String>) -> Self {
        self.with_entry("pushurl", url)
    }

    pub fn with_fetch_refspec(self, refspec: impl Into<String>) -> Self {
        self.with_entry("fetch", refspec)
    }
}

/// A whole-section removal for `[remote "<name>"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfigRemove {
    pub name: String,
}

impl RemoteConfigRemove {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

/// Source-aware config planning/apply errors.
#[derive(Debug)]
pub enum ConfigEditError {
    NoEditableSource,
    AmbiguousSource(Vec<ConfigSource>),
    RefusesExternalInclude {
        path: PathBuf,
    },
    IncludeIfNotSatisfied,
    SectionNotFound {
        section: String,
        subsection: Option<String>,
    },
    Parse(String),
    Locked {
        path: PathBuf,
    },
    Io(std::io::Error),
}

impl fmt::Display for ConfigEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoEditableSource => f.write_str("no editable config source"),
            Self::AmbiguousSource(sources) => {
                write!(f, "ambiguous config source among {} sources", sources.len())
            }
            Self::RefusesExternalInclude { path } => {
                write!(
                    f,
                    "refusing to edit external included config {}",
                    path.display()
                )
            }
            Self::IncludeIfNotSatisfied => f.write_str("includeIf condition is not satisfied"),
            Self::SectionNotFound {
                section,
                subsection,
            } => match subsection {
                Some(subsection) => {
                    write!(f, "config section not found: {section}.{subsection}")
                }
                None => write!(f, "config section not found: {section}"),
            },
            Self::Parse(message) => write!(f, "config parse error: {message}"),
            Self::Locked { path } => write!(f, "config lock already exists: {}", path.display()),
            Self::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

impl std::error::Error for ConfigEditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl Repository {
    /// Return the effective config as a source-attributed flat stream.
    pub fn config_with_sources(&self) -> Result<ConfigSnapshot, ConfigEditError> {
        let stack = self.config_stack_with_sources()?;
        Ok(self.config_snapshot_from_stack(stack))
    }

    /// Build an effective Git config stack with editable-origin metadata.
    pub fn config_stack(
        &self,
        options: ConfigStackOptions,
    ) -> Result<ConfigStackView, ConfigEditError> {
        let stack = self.config_stack_with_options(options)?;
        let values = self.config_snapshot_from_stack(stack.clone());
        let remotes = self.remote_config_snapshot_from_stack(stack);
        let mut section_sources = Vec::new();
        for remote in &remotes.remotes {
            for source in &remote.sources {
                if !section_sources.contains(source) {
                    section_sources.push(source.clone());
                }
            }
        }
        Ok(ConfigStackView {
            values,
            remotes,
            section_sources,
        })
    }

    fn config_snapshot_from_stack(&self, stack: ConfigStack) -> ConfigSnapshot {
        let bases = ConfigSourceBases::for_repository(self);
        let values = stack
            .entries
            .into_iter()
            .map(|entry| ConfigValue {
                key: config_entry_key(&entry.section, entry.subsection.as_deref(), &entry.key),
                value: entry.value,
                source: bases.source_for(entry.scope, &entry.origin, entry.included_from.as_ref()),
            })
            .collect();
        ConfigSnapshot { values }
    }

    /// Return configured remotes with physical source and editability metadata.
    pub fn remote_config_with_sources(&self) -> Result<RemoteConfigSnapshot, ConfigEditError> {
        let stack = self.config_stack_with_sources()?;
        Ok(self.remote_config_snapshot_from_stack(stack))
    }

    fn remote_config_snapshot_from_stack(&self, stack: ConfigStack) -> RemoteConfigSnapshot {
        let bases = ConfigSourceBases::for_repository(self);
        let mut remotes: Vec<RemoteConfig> = Vec::new();
        for entry in stack.entries {
            if !entry.section.eq_ignore_ascii_case("remote") {
                continue;
            }
            let Some(remote_name) = entry.subsection else {
                continue;
            };
            let source = bases.source_for(entry.scope, &entry.origin, entry.included_from.as_ref());
            let value = RemoteConfigValue {
                name: entry.key,
                value: entry.value,
                source: source.clone(),
            };
            let idx = match remotes.iter().position(|remote| remote.name == remote_name) {
                Some(idx) => idx,
                None => {
                    remotes.push(RemoteConfig {
                        name: remote_name.clone(),
                        values: Vec::new(),
                        sources: Vec::new(),
                    });
                    remotes.len() - 1
                }
            };
            let remote = &mut remotes[idx];
            if !remote.sources.iter().any(|item| item.source == source) {
                remote.sources.push(self.remote_source_metadata(source));
            }
            remote.values.push(value);
        }
        RemoteConfigSnapshot { remotes }
    }

    /// Plan the physical config file that should be edited for `key` and `scope`.
    ///
    /// The returned plan has no operations; callers may add operations directly
    /// or use [`Repository::plan_config_set`] / [`Repository::plan_config_unset`].
    pub fn plan_config_edit(
        &self,
        key: &str,
        scope: ConfigEditScope,
    ) -> Result<ConfigEditPlan, ConfigEditError> {
        let key = ParsedConfigKey::parse(key)?;
        let target_path = self.config_edit_target_path(&key, scope)?;
        Ok(ConfigEditPlan::new(target_path))
    }

    /// Plan a `set` operation for `key`.
    pub fn plan_config_set(
        &self,
        key: &str,
        value: impl Into<String>,
        scope: ConfigEditScope,
    ) -> Result<ConfigEditPlan, ConfigEditError> {
        Ok(self
            .plan_config_edit(key, scope)?
            .with_operation(ConfigEdit::set(key, value)?))
    }

    /// Plan an `unset` operation for `key`.
    pub fn plan_config_unset(
        &self,
        key: &str,
        scope: ConfigEditScope,
    ) -> Result<ConfigEditPlan, ConfigEditError> {
        Ok(self
            .plan_config_edit(key, scope)?
            .with_operation(ConfigEdit::unset(key)?))
    }

    /// Plan a whole-section replacement for `[remote "<name>"]`.
    ///
    /// The applied edit removes every matching remote section from the selected
    /// physical config file and appends one canonical replacement section.
    pub fn plan_remote_set(
        &self,
        set: RemoteConfigSet,
        scope: ConfigEditScope,
    ) -> Result<ConfigEditPlan, ConfigEditError> {
        validate_remote_name(&set.name)?;
        validate_section_entries("remote", Some(&set.name), &set.entries)?;
        let target_path = self.remote_config_edit_target_path(&set.name, scope)?;
        Ok(
            ConfigEditPlan::new(target_path).with_operation(ConfigEdit::replace_section(
                "remote",
                Some(set.name),
                set.entries,
            )),
        )
    }

    /// Plan removal of all `[remote "<name>"]` sections in the selected file.
    pub fn plan_remote_remove(
        &self,
        remove: RemoteConfigRemove,
        scope: ConfigEditScope,
    ) -> Result<ConfigEditPlan, ConfigEditError> {
        validate_remote_name(&remove.name)?;
        let target_path = self.remote_config_edit_target_path(&remove.name, scope)?;
        Ok(ConfigEditPlan::new(target_path)
            .with_operation(ConfigEdit::remove_section("remote", Some(remove.name))))
    }

    /// Apply a config edit plan through `<target>.lock` and atomic rename.
    pub fn apply_config_edit_plan(&self, plan: ConfigEditPlan) -> Result<(), ConfigEditError> {
        if plan.operations.is_empty() {
            return Ok(());
        }
        let mut contents = match fs::read(&plan.target_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(ConfigEditError::Io(err)),
        };
        for operation in plan.operations {
            let (section, subsection, name, value) = match operation {
                ConfigEdit::Set {
                    section,
                    subsection,
                    name,
                    value,
                } => (section, subsection, name, Some(value)),
                ConfigEdit::Unset {
                    section,
                    subsection,
                    name,
                } => (section, subsection, name, None),
                ConfigEdit::ReplaceSection {
                    section,
                    subsection,
                    entries,
                } => {
                    contents =
                        replace_section(contents, &section, subsection.as_deref(), &entries)?;
                    continue;
                }
                ConfigEdit::RemoveSection {
                    section,
                    subsection,
                } => {
                    contents = remove_section(contents, &section, subsection.as_deref())?;
                    continue;
                }
            };
            let mut editor = RawConfigEditor::new(contents, &section, subsection.as_deref(), &name);
            match editor.set_multivar(value.as_deref(), None, None, true) {
                RawEditOutcome::Changed => contents = editor.into_bytes(),
                RawEditOutcome::NothingSet => return Err(ConfigEditError::NoEditableSource),
            }
        }
        write_config_file_locked(
            &plan.target_path,
            &contents,
            ConfigFileWriteOptions { fsync: plan.fsync },
        )
        .map_err(ConfigEditError::from_config_write)
    }

    fn config_stack_with_sources(&self) -> Result<ConfigStack, ConfigEditError> {
        self.config_stack_with_options(ConfigStackOptions::git_default())
    }

    fn config_stack_with_options(
        &self,
        options: ConfigStackOptions,
    ) -> Result<ConfigStack, ConfigEditError> {
        let context = sley_config::ConfigIncludeContext::new(
            Some(self.config_include_git_dir()),
            self.config_include_branch(),
        );
        let mut stack = ConfigStack::new();
        for (path, scope) in sley_config::default_config_layer_paths() {
            stack
                .push_file(&path, scope, options.follow_includes, &context)
                .map_err(ConfigEditError::from_git_error)?;
        }
        stack
            .push_file(
                &self.common_dir().join("config"),
                ConfigScope::Local,
                options.follow_includes,
                &context,
            )
            .map_err(ConfigEditError::from_git_error)?;
        let include_worktree = match options.worktree_config {
            WorktreeConfig::Never => false,
            WorktreeConfig::Always => true,
            WorktreeConfig::WhenEnabled => self.worktree_config_enabled()?,
        };
        if include_worktree {
            stack
                .push_file(
                    &self.git_dir().join("config.worktree"),
                    ConfigScope::Worktree,
                    options.follow_includes,
                    &context,
                )
                .map_err(ConfigEditError::from_git_error)?;
        }
        Ok(stack)
    }

    fn worktree_config_enabled(&self) -> Result<bool, ConfigEditError> {
        let path = self.common_dir().join("config");
        match crate::GitConfig::read(&path) {
            Ok(config) => Ok(config
                .get_bool("extensions", None, "worktreeConfig")
                .unwrap_or(false)),
            Err(GitError::Io(_)) | Err(GitError::NotFound(_)) => Ok(false),
            Err(err) => Err(ConfigEditError::from_git_error(err)),
        }
    }

    fn config_edit_target_path(
        &self,
        key: &ParsedConfigKey,
        scope: ConfigEditScope,
    ) -> Result<PathBuf, ConfigEditError> {
        match scope {
            ConfigEditScope::Local => Ok(self.common_dir().join("config")),
            ConfigEditScope::Worktree => Ok(self.git_dir().join("config.worktree")),
            ConfigEditScope::Global => sley_config::default_config_layer_paths()
                .into_iter()
                .filter_map(|(path, scope)| (scope == ConfigScope::Global).then_some(path))
                .next_back()
                .ok_or(ConfigEditError::NoEditableSource),
            ConfigEditScope::System => sley_config::default_config_layer_paths()
                .into_iter()
                .find_map(|(path, scope)| (scope == ConfigScope::System).then_some(path))
                .ok_or(ConfigEditError::NoEditableSource),
            ConfigEditScope::Path(path) => Ok(path),
            ConfigEditScope::ExistingValue {
                allow_external_includes,
            } => {
                let snapshot = self.config_with_sources()?;
                let Some(value) = snapshot
                    .values
                    .iter()
                    .rev()
                    .find(|value| value.key == key.full)
                else {
                    return Err(ConfigEditError::NoEditableSource);
                };
                self.edit_path_for_source(&value.source, allow_external_includes)
            }
        }
    }

    fn remote_config_edit_target_path(
        &self,
        name: &str,
        scope: ConfigEditScope,
    ) -> Result<PathBuf, ConfigEditError> {
        match scope {
            ConfigEditScope::ExistingValue {
                allow_external_includes,
            } => {
                let stack = self.config_stack_with_sources()?;
                let bases = ConfigSourceBases::for_repository(self);
                for entry in stack.entries.iter().rev() {
                    if entry.section.eq_ignore_ascii_case("remote")
                        && entry.subsection.as_deref() == Some(name)
                    {
                        let source = bases.source_for(
                            entry.scope,
                            &entry.origin,
                            entry.included_from.as_ref(),
                        );
                        return self.edit_path_for_source(&source, allow_external_includes);
                    }
                }
                Err(ConfigEditError::NoEditableSource)
            }
            other => {
                let key = ParsedConfigKey::parse(&format!("remote.{name}.url"))?;
                self.config_edit_target_path(&key, other)
            }
        }
    }

    fn edit_path_for_source(
        &self,
        source: &ConfigSource,
        allow_external_includes: bool,
    ) -> Result<PathBuf, ConfigEditError> {
        match source {
            ConfigSource::Local { path }
            | ConfigSource::Worktree { path }
            | ConfigSource::Global { path }
            | ConfigSource::System { path } => Ok(path.clone()),
            ConfigSource::Included { path, .. } => {
                if allow_external_includes || self.config_path_is_inside_repository(path) {
                    Ok(path.clone())
                } else {
                    Err(ConfigEditError::RefusesExternalInclude { path: path.clone() })
                }
            }
            ConfigSource::Env
            | ConfigSource::CommandLine
            | ConfigSource::Blob { .. }
            | ConfigSource::Stdin => Err(ConfigEditError::NoEditableSource),
        }
    }

    fn config_path_is_inside_repository(&self, path: &Path) -> bool {
        let mut roots = self
            .workdir()
            .into_iter()
            .chain(std::iter::once(self.common_dir().to_path_buf()));
        roots.any(|root| path_starts_with(path, &root))
    }

    fn remote_source_metadata(&self, source: ConfigSource) -> RemoteConfigSource {
        match self.edit_path_for_source(&source, false) {
            Ok(path) => RemoteConfigSource {
                source,
                editable: true,
                target_path: Some(path),
                refusal: None,
            },
            Err(ConfigEditError::RefusesExternalInclude { path }) => RemoteConfigSource {
                source,
                editable: false,
                target_path: None,
                refusal: Some(RemoteConfigRefusal::ExternalInclude { path }),
            },
            Err(_) => RemoteConfigSource {
                source,
                editable: false,
                target_path: None,
                refusal: Some(RemoteConfigRefusal::SyntheticSource),
            },
        }
    }
}

struct ConfigSourceBases {
    local: PathBuf,
    worktree: PathBuf,
    globals: Vec<PathBuf>,
    system: Option<PathBuf>,
}

impl ConfigSourceBases {
    fn for_repository(repo: &Repository) -> Self {
        let mut globals = Vec::new();
        let mut system = None;
        for (path, scope) in sley_config::default_config_layer_paths() {
            match scope {
                ConfigScope::System => system = Some(path),
                ConfigScope::Global => globals.push(path),
                _ => {}
            }
        }
        Self {
            local: repo.common_dir().join("config"),
            worktree: repo.git_dir().join("config.worktree"),
            globals,
            system,
        }
    }

    fn source_for(
        &self,
        scope: ConfigScope,
        origin: &sley_config::ConfigOrigin,
        included_from: Option<&sley_config::ConfigOrigin>,
    ) -> ConfigSource {
        match origin.kind {
            ConfigOriginKind::File => {
                let path = PathBuf::from(&origin.name);
                match scope {
                    ConfigScope::Local if paths_equivalent(&path, &self.local) => {
                        ConfigSource::Local { path }
                    }
                    ConfigScope::Worktree if paths_equivalent(&path, &self.worktree) => {
                        ConfigSource::Worktree { path }
                    }
                    ConfigScope::Global
                        if self
                            .globals
                            .iter()
                            .any(|base| paths_equivalent(&path, base)) =>
                    {
                        ConfigSource::Global { path }
                    }
                    ConfigScope::System
                        if self
                            .system
                            .as_ref()
                            .is_some_and(|base| paths_equivalent(&path, base)) =>
                    {
                        ConfigSource::System { path }
                    }
                    _ => ConfigSource::Included {
                        path,
                        included_from: included_from.and_then(file_origin_path),
                    },
                }
            }
            ConfigOriginKind::Blob => ConfigSource::Blob {
                spec: origin.name.clone(),
            },
            ConfigOriginKind::Stdin => ConfigSource::Stdin,
            ConfigOriginKind::CommandLine => ConfigSource::CommandLine,
        }
    }
}

fn file_origin_path(origin: &sley_config::ConfigOrigin) -> Option<PathBuf> {
    (origin.kind == ConfigOriginKind::File).then(|| PathBuf::from(&origin.name))
}

#[derive(Debug, Clone)]
struct ParsedConfigKey {
    full: String,
    section: String,
    subsection: Option<String>,
    name: String,
}

impl ParsedConfigKey {
    fn parse(key: &str) -> Result<Self, ConfigEditError> {
        let full = sley_config::canonicalize_config_key(key)
            .map_err(|err| ConfigEditError::Parse(err.message()))?;
        let first_dot = full
            .find('.')
            .ok_or_else(|| ConfigEditError::Parse(format!("invalid config key: {key}")))?;
        let last_dot = full
            .rfind('.')
            .ok_or_else(|| ConfigEditError::Parse(format!("invalid config key: {key}")))?;
        let section = full[..first_dot].to_string();
        let name = full[last_dot + 1..].to_string();
        let subsection = (first_dot != last_dot).then(|| full[first_dot + 1..last_dot].to_string());
        Ok(Self {
            full,
            section,
            subsection,
            name,
        })
    }
}

fn config_entry_key(section: &str, subsection: Option<&str>, name: &str) -> String {
    let section = section.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    match subsection {
        Some(subsection) => format!("{section}.{subsection}.{name}"),
        None => format!("{section}.{name}"),
    }
}

fn validate_remote_name(name: &str) -> Result<(), ConfigEditError> {
    if name.is_empty() {
        return Err(ConfigEditError::Parse(
            "remote name must not be empty".into(),
        ));
    }
    ParsedConfigKey::parse(&format!("remote.{name}.url")).map(|_| ())
}

fn validate_section_entries(
    section: &str,
    subsection: Option<&str>,
    entries: &[ConfigSectionEntry],
) -> Result<(), ConfigEditError> {
    for entry in entries {
        if entry.name.is_empty() {
            return Err(ConfigEditError::Parse(
                "config entry name must not be empty".into(),
            ));
        }
        let key = match subsection {
            Some(subsection) => format!("{section}.{subsection}.{}", entry.name),
            None => format!("{section}.{}", entry.name),
        };
        ParsedConfigKey::parse(&key)?;
    }
    Ok(())
}

fn replace_section(
    contents: Vec<u8>,
    section: &str,
    subsection: Option<&str>,
    entries: &[ConfigSectionEntry],
) -> Result<Vec<u8>, ConfigEditError> {
    let contents = match remove_section_if_present(&contents, section, subsection)? {
        Some(contents) => contents,
        None => contents,
    };
    Ok(append_section(contents, section, subsection, entries))
}

fn remove_section(
    contents: Vec<u8>,
    section: &str,
    subsection: Option<&str>,
) -> Result<Vec<u8>, ConfigEditError> {
    match remove_section_if_present(&contents, section, subsection)? {
        Some(contents) => Ok(contents),
        None => Err(ConfigEditError::SectionNotFound {
            section: section.to_string(),
            subsection: subsection.map(str::to_string),
        }),
    }
}

fn remove_section_if_present(
    contents: &[u8],
    section: &str,
    subsection: Option<&str>,
) -> Result<Option<Vec<u8>>, ConfigEditError> {
    let name = normalised_section_name(section, subsection);
    match rename_or_remove_section(contents, &name, None) {
        SectionEditOutcome::Changed(contents) => Ok(Some(contents)),
        SectionEditOutcome::NotFound => Ok(None),
        SectionEditOutcome::LineTooLong(line) => Err(ConfigEditError::Parse(format!(
            "config line {line} exceeds maximum line length"
        ))),
    }
}

fn append_section(
    mut contents: Vec<u8>,
    section: &str,
    subsection: Option<&str>,
    entries: &[ConfigSectionEntry],
) -> Vec<u8> {
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }
    render_section(&mut contents, section, subsection, entries);
    contents
}

fn render_section(
    out: &mut Vec<u8>,
    section: &str,
    subsection: Option<&str>,
    entries: &[ConfigSectionEntry],
) {
    out.push(b'[');
    out.extend_from_slice(section.as_bytes());
    if let Some(subsection) = subsection {
        out.extend_from_slice(b" \"");
        for ch in subsection.chars() {
            if ch == '"' || ch == '\\' {
                out.push(b'\\');
            }
            let mut buf = [0; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        out.push(b'"');
    }
    out.extend_from_slice(b"]\n");
    for entry in entries {
        out.push(b'\t');
        out.extend_from_slice(entry.name.as_bytes());
        if let Some(value) = &entry.value {
            out.extend_from_slice(b" = ");
            out.extend_from_slice(quote_config_value(value).as_bytes());
        }
        out.push(b'\n');
    }
}

fn quote_config_value(value: &str) -> String {
    let needs_quotes = value.starts_with(' ')
        || value.ends_with(' ')
        || value.contains('\r')
        || value.bytes().any(|byte| matches!(byte, b'#' | b';'));
    let mut out = String::new();
    if needs_quotes {
        out.push('"');
    }
    for ch in value.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    if needs_quotes {
        out.push('"');
    }
    out
}

fn normalised_section_name(section: &str, subsection: Option<&str>) -> String {
    match subsection {
        Some(subsection) => format!("{}.{subsection}", section.to_ascii_lowercase()),
        None => section.to_ascii_lowercase(),
    }
}

fn paths_equivalent(left: &Path, right: &Path) -> bool {
    left == right
        || match (fs::canonicalize(left), fs::canonicalize(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    if path.starts_with(root) {
        return true;
    }
    match (fs::canonicalize(path), fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => false,
    }
}

impl ConfigEditError {
    fn from_git_error(err: GitError) -> Self {
        match err {
            GitError::Io(message) => Self::Io(std::io::Error::other(message)),
            GitError::InvalidFormat(message)
            | GitError::InvalidPath(message)
            | GitError::InvalidObject(message)
            | GitError::InvalidObjectId(message)
            | GitError::Unsupported(message) => Self::Parse(message),
            other => Self::Parse(other.to_string()),
        }
    }

    fn from_config_write(err: ConfigFileWriteError) -> Self {
        match err {
            ConfigFileWriteError::ExistingLock(path) => Self::Locked { path },
            ConfigFileWriteError::Io { source, .. } => Self::Io(source),
        }
    }
}
