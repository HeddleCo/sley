//! Capability probes for embedders (e.g. heddle) that need to know what the
//! linked sley build can do before calling into it.

use crate::Repository;
use sley_core::ObjectFormat;

#[cfg(feature = "remote")]
use sley_remote::TransportCapabilities;

/// Describes what this repository and the linked sley engine support.
///
/// Most fields reflect library-wide support; [`RepositoryCapabilities::shallow`]
/// and [`RepositoryCapabilities::sha256`] are additionally adjusted per
/// repository when obtained via [`Repository::capabilities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepositoryCapabilities {
    /// Git notes read/write (tree-backed `refs/notes/*`) via [`sley_notes`].
    pub notes: bool,
    /// Annotated tag object creation ([`Repository::write_annotated_tag`]).
    pub annotated_tags: bool,
    /// `include` / `includeIf` config directive resolution.
    pub config_includes: bool,
    /// `includeIf "hasconfig:…"` conditional includes.
    pub hasconfig_include_if: bool,
    /// Index v2/v3/v4 read and write.
    pub index: bool,
    /// Shallow repositories (`shallow` file, shallow fetch/clone).
    pub shallow: bool,
    /// SHA-256 object format support for this repository.
    pub sha256: bool,
}

impl RepositoryCapabilities {
    /// Capabilities of the currently linked sley build, before per-repo tweaks.
    pub const fn current() -> Self {
        Self {
            notes: true,
            annotated_tags: true,
            config_includes: true,
            hasconfig_include_if: true,
            index: true,
            shallow: true,
            sha256: true,
        }
    }
}

impl Repository {
    /// Report what this repository and the linked engine support.
    pub fn capabilities(&self) -> RepositoryCapabilities {
        let mut caps = RepositoryCapabilities::current();
        caps.shallow = self.is_shallow();
        caps.sha256 = self.object_format() == ObjectFormat::Sha256;
        caps
    }

    /// Transport capabilities when the `remote` feature is enabled.
    #[cfg(feature = "remote")]
    pub fn transport_capabilities(&self) -> TransportCapabilities {
        TransportCapabilities::current()
    }
}
