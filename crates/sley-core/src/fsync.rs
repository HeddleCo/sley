//! Repository durability policy: `core.fsync` component selection and
//! `core.fsyncMethod` barrier choice.
//!
//! Promoted from `sley-refs` so every writer resolves one shared policy
//! instead of hand-rolling barrier decisions. Mirrors upstream git 2.55:
//!
//! - component bit layout and aggregate groups (`write-or-die.h`),
//! - the `core.fsync` grammar with negation accumulation order and
//!   prefix matching (`environment.c` `parse_fsync_components`),
//! - method selection including the platform default (`FSYNC_METHOD_DEFAULT`;
//!   sley keeps the Windows `batch` mapping used by its reference store), and
//! - the `GIT_TEST_FSYNC` kill switch (`write-or-die.c` `maybe_fsync`,
//!   default enabled).
//!
//! Token handling follows the established sley grammar (`sley-refs`
//! `core_fsync_includes_reference`): comma-separated components are trimmed,
//! so unlike upstream's raw `strspn`/`strncmp` scan a trailing space inside a
//! token still matches. Unknown components are ignored.

use std::env;
use std::fs;
use std::io;
use std::path::Path;

/// The set of repository parts to harden through an [`FsyncMethod`] barrier,
/// mirroring upstream `enum fsync_component` (git 2.55 `write-or-die.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FsyncComponents(u32);

impl FsyncComponents {
    /// Upstream `FSYNC_COMPONENT_NONE`: data that is not persistent and must
    /// never be synced.
    pub const NONE: Self = Self(0);
    pub const LOOSE_OBJECT: Self = Self(1 << 0);
    pub const PACK: Self = Self(1 << 1);
    pub const PACK_METADATA: Self = Self(1 << 2);
    pub const COMMIT_GRAPH: Self = Self(1 << 3);
    pub const INDEX: Self = Self(1 << 4);
    pub const REFERENCE: Self = Self(1 << 5);
    pub const OBJECT_MAP: Self = Self(1 << 6);

    /// Upstream `FSYNC_COMPONENTS_OBJECTS`.
    pub const OBJECTS: Self = Self(Self::LOOSE_OBJECT.0 | Self::PACK.0);

    /// Upstream `FSYNC_COMPONENTS_DERIVED_METADATA`.
    pub const DERIVED_METADATA: Self = Self(Self::PACK_METADATA.0 | Self::COMMIT_GRAPH.0);

    /// Upstream `FSYNC_COMPONENTS_DEFAULT`: everything except loose objects.
    pub const DEFAULT: Self = Self(
        (Self::OBJECTS.0 | Self::DERIVED_METADATA.0) & !Self::LOOSE_OBJECT.0,
    );

    /// Upstream `FSYNC_COMPONENTS_COMMITTED`.
    pub const COMMITTED: Self = Self(Self::OBJECTS.0 | Self::REFERENCE.0);

    /// Upstream `FSYNC_COMPONENTS_ADDED`.
    pub const ADDED: Self = Self(Self::COMMITTED.0 | Self::INDEX.0);

    /// Upstream `FSYNC_COMPONENTS_ALL`.
    pub const ALL: Self = Self(
        Self::LOOSE_OBJECT.0
            | Self::PACK.0
            | Self::PACK_METADATA.0
            | Self::COMMIT_GRAPH.0
            | Self::INDEX.0
            | Self::REFERENCE.0
            | Self::OBJECT_MAP.0,
    );

    /// Upstream `FSYNC_COMPONENTS_PLATFORM_DEFAULT`. No platform overrides it
    /// in git v2.55, so this equals [`Self::DEFAULT`] everywhere today; kept
    /// separate because git exposes the default as compile-time platform
    /// policy and `parse` starts from it.
    pub const PLATFORM_DEFAULT: Self = Self::DEFAULT;

    /// `(name, bits)` rows in upstream's `fsync_component_names` order. Group
    /// names expand here rather than at parse time, matching upstream.
    const COMPONENT_TABLE: [(&str, Self); 11] = [
        ("loose-object", Self::LOOSE_OBJECT),
        ("pack", Self::PACK),
        ("pack-metadata", Self::PACK_METADATA),
        ("commit-graph", Self::COMMIT_GRAPH),
        ("index", Self::INDEX),
        ("objects", Self::OBJECTS),
        ("reference", Self::REFERENCE),
        ("derived-metadata", Self::DERIVED_METADATA),
        ("committed", Self::COMMITTED),
        ("added", Self::ADDED),
        ("all", Self::ALL),
    ];

    /// Parse one `core.fsync` value into a component set.
    ///
    /// Grammar (upstream `parse_fsync_components`): start from
    /// [`Self::PLATFORM_DEFAULT`]; `none` resets the running base to empty;
    /// each remaining comma-separated component is trimmed and prefix-matched
    /// against `Self::COMPONENT_TABLE`, accumulating into negative or
    /// positive masks by leading `-`; finally the result is
    /// `(base & ~negative) | positive`, so a component named both ways wins
    /// as positive in either order. Unknown components are ignored, and a
    /// bare `-` ends parsing like upstream's warning path.
    pub fn parse(value: &str) -> Self {
        let mut current = Self::PLATFORM_DEFAULT;
        let mut positive = Self::NONE;
        let mut negative = Self::NONE;
        for raw_component in value.split(',') {
            let component = raw_component.trim();
            if component == "none" {
                current = Self::NONE;
                continue;
            }
            if component.is_empty() {
                continue;
            }
            let Some(name) = component.strip_prefix('-') else {
                for (table_name, bits) in Self::COMPONENT_TABLE {
                    if table_name.starts_with(component) {
                        positive = positive.union(bits);
                    }
                }
                continue;
            };
            if name.is_empty() {
                break;
            }
            for (table_name, bits) in Self::COMPONENT_TABLE {
                if table_name.starts_with(name) {
                    negative = negative.union(bits);
                }
            }
        }
        current.without(negative).union(positive)
    }

    /// Whether every bit of `other` is present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Union with `other`.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Remove all bits of `other`.
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Raw bitmask, mainly for tests and diagnostics.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether reference files are included, replacing the old
    /// `core_fsync_includes_reference` predicate on equal terms.
    pub const fn includes_reference(self) -> bool {
        self.contains(Self::REFERENCE)
    }
}

/// Barrier implementation backing `core.fsyncMethod`, mirroring upstream
/// `enum fsync_method`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncMethod {
    /// `fsync`: full data + metadata flush (`fsync(2)` / `FlushFileBuffers`).
    Fsync,
    /// `writeout-only`: page-cache writeback without a hardware cache flush.
    WriteoutOnly,
    /// `batch`: writeout-only staging with one hardware flush per operation
    /// (treated identically to [`FsyncMethod::Fsync`] outside bulk checkin).
    Batch,
}

impl FsyncMethod {
    /// Upstream `FSYNC_METHOD_DEFAULT`: `writeout-only` on Apple platforms,
    /// `batch` on Windows (upstream Min builds flush via `FlushFileBuffers`),
    /// full `fsync` elsewhere.
    pub const fn platform_default() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::Batch
        }
        #[cfg(all(not(target_os = "windows"), target_os = "macos"))]
        {
            Self::WriteoutOnly
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            Self::Fsync
        }
    }

    /// Map a `core.fsyncMethod` value; unknown or absent values fall back to
    /// the platform default (matching `ReferenceFsyncMethod::from_config`).
    pub fn from_config(value: Option<&str>) -> Self {
        match value {
            Some("fsync") => Self::Fsync,
            Some("writeout-only") => Self::WriteoutOnly,
            Some("batch") => Self::Batch,
            _ => Self::platform_default(),
        }
    }

    /// Apply this method's barrier to an open file handle. Mirrors the
    /// `maybe_fsync` dispatch used by the reference store: `writeout-only`
    /// maps to `sync_data`, the other methods to `sync_all`.
    pub fn apply(self, file: &fs::File) -> io::Result<()> {
        match self {
            Self::WriteoutOnly => file.sync_data(),
            Self::Fsync | Self::Batch => file.sync_all(),
        }
    }
}

/// Whether `GIT_TEST_FSYNC` permits real barriers. Upstream reads this as a
/// boolean through `git_env_bool("GIT_TEST_FSYNC", 1)` inside `maybe_fsync`;
/// unrecognized spellings leave syncing enabled.
pub fn test_fsync_enabled() -> bool {
    let Ok(value) = env::var("GIT_TEST_FSYNC") else {
        return true;
    };
    !matches!(
        value.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off" | ""
    )
}

/// Minimal read-only config lookup surface accepted by [`Policy::resolve`].
///
/// Defined here — not as a concrete `GitConfig` parameter — because
/// `sley-config` depends on this crate, not the other way around. The config
/// crate implements this over `GitConfig::get`, so callers holding resolved
/// configuration pass it directly to [`Policy::resolve`].
pub trait FsyncConfigSource {
    /// Last value of `<section>.<subsection?>.<key>` (case-normalized
    /// lookup), or `None` when unset.
    fn fsync_lookup(&self, section: &str, subsection: Option<&str>, key: &str) -> Option<&str>;
}

/// Resolved durability policy for repository writes.
///
/// Combines the component selection from `core.fsync`, the barrier choice
/// from `core.fsyncMethod`, and the `GIT_TEST_FSYNC` gate, in the shape of
/// upstream's `fsync_components` + `fsync_method` + `use_fsync` globals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    components: FsyncComponents,
    method: FsyncMethod,
    use_fsync: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self::from_values(None, None)
    }
}

impl Policy {
    /// Resolve from raw `core.fsync` / `core.fsyncMethod` values. Absent
    /// values leave the corresponding platform default in place.
    pub fn from_values(core_fsync: Option<&str>, core_fsync_method: Option<&str>) -> Self {
        Self {
            components: core_fsync.map_or(FsyncComponents::PLATFORM_DEFAULT, FsyncComponents::parse),
            method: FsyncMethod::from_config(core_fsync_method),
            use_fsync: test_fsync_enabled(),
        }
    }

    /// Resolve from parsed configuration (e.g. `GitConfig`). Configuration
    /// lookup failures should fall back to [`Policy::default`], which matches
    /// reading a repository without `core.fsync` keys.
    pub fn resolve(config: &impl FsyncConfigSource) -> Self {
        Self::from_values(
            config.fsync_lookup("core", None, "fsync"),
            config.fsync_lookup("core", None, "fsyncMethod"),
        )
    }

    /// Apply command-line-style overrides (`git -c core.fsync=...`): `Some`
    /// values replace the corresponding setting, `None` keeps it, and the
    /// test switch is re-read afterwards so callers holding a long-lived
    /// policy observe current environment state.
    pub fn overridden(mut self, core_fsync: Option<&str>, core_fsync_method: Option<&str>) -> Self {
        if let Some(value) = core_fsync {
            self.components = FsyncComponents::parse(value);
        }
        if let Some(value) = core_fsync_method {
            self.method = FsyncMethod::from_config(Some(value));
        }
        self.use_fsync = test_fsync_enabled();
        self
    }

    /// Component set after `core.fsync` parsing.
    pub const fn components(&self) -> FsyncComponents {
        self.components
    }

    /// Effective barrier method.
    pub const fn method(&self) -> FsyncMethod {
        self.method
    }

    /// Whether writes to `component` take a barrier under this policy
    /// (upstream `fsync_component()`'s gating, including the test switch).
    pub const fn syncs(&self, component: FsyncComponents) -> bool {
        self.use_fsync && self.components.contains(component)
    }

    /// The barrier method to apply when writing `component` files, or `None`
    /// when no barrier applies. This is the shape threaded through locked
    /// write paths that sync before rename.
    pub const fn method_if_enabled(&self, component: FsyncComponents) -> Option<FsyncMethod> {
        if self.syncs(component) {
            Some(self.method)
        } else {
            None
        }
    }

    /// Sync an open file handle when `component` is covered by this policy;
    /// otherwise return without touching the handle. Errors surface verbatim.
    pub fn apply(&self, file: &fs::File, component: FsyncComponents) -> io::Result<()> {
        match self.method_if_enabled(component) {
            Some(method) => method.apply(file),
            None => Ok(()),
        }
    }
}

/// Open `path` for writing (no truncate, no create) and apply `policy`'s
/// barrier for `component`. Convenience for post-hoc hardening of already
/// published files; write paths that hold the handle open should prefer
/// [`Policy::apply`] directly. Requires write access because `sync_all`
/// degrades to a permission error on read-only handles (Windows
/// `FlushFileBuffers` semantics).
pub fn sync_file(path: &Path, policy: &Policy, component: FsyncComponents) -> io::Result<()> {
    let file = fs::OpenOptions::new().write(true).open(path)?;
    policy.apply(&file, component)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_bits_match_upstream_layout() {
        assert_eq!(FsyncComponents::OBJECTS.bits(), 0b0000_0011);
        assert_eq!(FsyncComponents::DERIVED_METADATA.bits(), 0b0000_1100);
        // DEFAULT = (objects | derived-metadata) & ~loose-object.
        assert_eq!(
            FsyncComponents::DEFAULT.bits(),
            FsyncComponents::PACK.bits()
                | FsyncComponents::PACK_METADATA.bits()
                | FsyncComponents::COMMIT_GRAPH.bits()
        );
        assert_eq!(
            FsyncComponents::COMMITTED.bits(),
            FsyncComponents::OBJECTS.union(FsyncComponents::REFERENCE).bits()
        );
        assert_eq!(
            FsyncComponents::ADDED.bits(),
            FsyncComponents::COMMITTED
                .union(FsyncComponents::INDEX)
                .bits()
        );
        assert_eq!(FsyncComponents::ALL.bits(), 0b0111_1111);
    }

    #[test]
    fn parse_matches_upstream_groups_negation_and_prefixing() {
        let reference = FsyncComponents::REFERENCE;
        assert!(!FsyncComponents::parse("none").contains(reference));
        // `none` clears the base but later components still accumulate.
        assert!(FsyncComponents::parse("none,reference").contains(reference));
        assert!(!FsyncComponents::parse("none,-reference").contains(reference));
        assert!(!FsyncComponents::parse("objects,index").contains(reference));
        assert!(!FsyncComponents::parse("-reference").contains(reference));
        for value in ["reference", "ref", "committed", "added", "all"] {
            assert!(
                FsyncComponents::parse(value).contains(reference),
                "{value} must include references"
            );
        }
        // Positives win over accumulated negatives in either order.
        assert!(FsyncComponents::parse("reference,-reference").contains(reference));
        assert!(FsyncComponents::parse("-reference,reference").contains(reference));
        // `none` resets the running base; earlier positives still apply.
        assert!(FsyncComponents::parse("reference,none").contains(reference));
        assert!(FsyncComponents::parse(
            "committed,-loose-object"
        )
        .contains(reference));
        // Prefix matching reaches aggregate rows: upstream's strncmp scan
        // makes "pack" also select pack-metadata.
        assert!(FsyncComponents::parse("pack").contains(FsyncComponents::PACK_METADATA));
        // Unknown components are ignored.
        assert_eq!(
            FsyncComponents::parse("nonsense").bits(),
            FsyncComponents::PLATFORM_DEFAULT.bits()
        );
    }

    #[test]
    fn policy_gating_honors_components_and_test_switch() {
        let enabled = Policy::from_values(Some("reference"), Some("writeout-only"));
        assert!(enabled.syncs(FsyncComponents::REFERENCE) || !test_fsync_enabled());
        if test_fsync_enabled() {
            assert_eq!(
                enabled.method_if_enabled(FsyncComponents::REFERENCE),
                Some(FsyncMethod::WriteoutOnly)
            );
            assert_eq!(enabled.method_if_enabled(FsyncComponents::INDEX), None);
        }

        let disabled = Policy::from_values(Some("none"), Some("fsync"));
        assert_eq!(disabled.method_if_enabled(FsyncComponents::REFERENCE), None);

        // Absent core.fsync leaves the platform default, which excludes
        // references on every git v2.55 platform.
        let default = Policy::from_values(None, None);
        assert!(!default.components().contains(FsyncComponents::REFERENCE));

        // Overrides replace only the provided values.
        let overridden = disabled.overridden(None, Some("batch"));
        assert_eq!(overridden.method(), FsyncMethod::Batch);
        let flipped = default.overridden(Some("all"), None);
        if test_fsync_enabled() {
            assert_eq!(
                flipped.method_if_enabled(FsyncComponents::REFERENCE),
                Some(FsyncMethod::platform_default())
            );
        }
    }
}
