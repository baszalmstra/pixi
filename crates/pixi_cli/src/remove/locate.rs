//! Locating where a dependency lives in a workspace manifest.
//!
//! This module walks every dependency table across all features, targets, and
//! spec types so that `pixi remove` can find a package without the user having
//! to specify `--pypi`, `--host`, `--build`, `--feature`, or `--platform`. The
//! same walker also powers the "did you mean ..." suggestions in
//! [`super::error`].

use std::str::FromStr;

use indexmap::IndexMap;
use pixi_consts::consts;
use pixi_core::DependencyType;
use pixi_manifest::{FeatureName, SpecType, TargetSelector, WorkspaceManifest};
use pixi_pypi_spec::PypiPackageName;
use rattler_conda_types::{PackageName, Platform};

/// Identifies which dependency table a package lives in. Mirrors
/// [`DependencyType`] but is `Hash`/`Eq` so locations can be deduplicated.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub(super) enum Slot {
    Conda(SpecType),
    Pypi,
}

impl From<DependencyType> for Slot {
    fn from(value: DependencyType) -> Self {
        match value {
            DependencyType::CondaDependency(s) => Slot::Conda(s),
            DependencyType::PypiDependency => Slot::Pypi,
        }
    }
}

impl Slot {
    pub(super) fn table_name(self) -> &'static str {
        match self {
            Slot::Pypi => "pypi-dependencies",
            Slot::Conda(SpecType::Host) => "host-dependencies",
            Slot::Conda(SpecType::Build) => "build-dependencies",
            Slot::Conda(SpecType::Run) => "dependencies",
            Slot::Conda(SpecType::RunConstraints) => "run-constraints",
        }
    }

    pub(super) fn cli_flag(self) -> Option<&'static str> {
        match self {
            Slot::Pypi => Some("--pypi"),
            Slot::Conda(SpecType::Host) => Some("--host"),
            Slot::Conda(SpecType::Build) => Some("--build"),
            // `Run` is the default; `RunConstraints` cannot be removed via the CLI.
            Slot::Conda(_) => None,
        }
    }
}

/// One dependency entry discovered while walking the workspace. Borrows the
/// name from the manifest so iterating allocates nothing.
pub(super) struct DepEntry<'a> {
    pub feature: &'a FeatureName,
    pub slot: Slot,
    /// The platform target the entry was found in. `None` is the
    /// platform-agnostic table (e.g. `[dependencies]`).
    pub platform: Option<Platform>,
    pub name: &'a str,
}

/// A distinct location (a single dependency table of a single feature) where a
/// package is defined, together with the platform scopes it appears in.
pub(super) struct Location {
    pub feature: FeatureName,
    pub slot: Slot,
    /// The platform scopes within this table where the package is defined.
    /// `None` is the platform-agnostic table.
    pub platforms: Vec<Option<Platform>>,
}

/// Walk every dependency entry across all features, targets, and spec types,
/// lazily and without allocating.
///
/// Only the platform-agnostic target (`None`) and concrete per-platform targets
/// are visited; group selectors (`linux`/`unix`/`win`/`osx`) are skipped
/// because they cannot be addressed through the CLI's `--platform` flag.
pub(super) fn walk_dependencies(
    manifest: &WorkspaceManifest,
) -> impl Iterator<Item = DepEntry<'_>> + '_ {
    let conda = manifest.iter_conda_dependencies().filter_map(|dep| {
        Some(DepEntry {
            feature: dep.feature,
            slot: Slot::Conda(dep.spec_type),
            platform: selector_platform(dep.selector)?,
            name: dep.name.as_normalized(),
        })
    });
    let pypi = manifest.iter_pypi_dependencies().filter_map(|dep| {
        Some(DepEntry {
            feature: dep.feature,
            slot: Slot::Pypi,
            platform: selector_platform(dep.selector)?,
            name: dep.name.as_source(),
        })
    });
    conda.chain(pypi)
}

/// Map a target selector to the platform scope used for removal, returning
/// `None` to skip group selectors (`linux`/`unix`/`win`/`osx`) that the CLI's
/// `--platform` flag cannot address.
fn selector_platform(selector: Option<&TargetSelector>) -> Option<Option<Platform>> {
    match selector {
        None => Some(None),
        Some(TargetSelector::Platform(p)) => Some(Some(*p)),
        Some(_) => None,
    }
}

/// Find every distinct location of `name` in the workspace, grouped by
/// `(feature, table)`. Each location records all platform scopes where the
/// package appears so the caller can remove them in one pass.
///
/// Insertion order follows [`walk_dependencies`], which is deterministic.
pub(super) fn locate(manifest: &WorkspaceManifest, name: &str) -> Vec<Location> {
    let target_conda = PackageName::try_from(name).ok();
    let target_pypi = PypiPackageName::from_str(name).ok();

    let mut groups: IndexMap<(FeatureName, Slot), Vec<Option<Platform>>> = IndexMap::new();
    for entry in walk_dependencies(manifest) {
        if is_exact_match(&entry, target_conda.as_ref(), target_pypi.as_ref()) {
            groups
                .entry((entry.feature.clone(), entry.slot))
                .or_default()
                .push(entry.platform);
        }
    }

    groups
        .into_iter()
        .map(|((feature, slot), platforms)| Location {
            feature,
            slot,
            platforms,
        })
        .collect()
}

pub(super) fn is_exact_match(
    entry: &DepEntry<'_>,
    target_conda: Option<&PackageName>,
    target_pypi: Option<&PypiPackageName>,
) -> bool {
    match entry.slot {
        Slot::Conda(_) => target_conda
            .and_then(|t| PackageName::try_from(entry.name).ok().map(|n| n == *t))
            .unwrap_or(false),
        Slot::Pypi => target_pypi
            .and_then(|t| PypiPackageName::from_str(entry.name).ok().map(|n| n == *t))
            .unwrap_or(false),
    }
}

/// Render a hint that points at a concrete location and the command to remove
/// the package from there.
pub(super) fn format_exact_location(name: &str, slot: Slot, feature: &FeatureName) -> String {
    let feature_part = if feature.is_default() {
        "the default feature".to_string()
    } else {
        format!("feature `{}`", consts::FEATURE_STYLE.apply_to(feature))
    };
    format!(
        "`{name}` is a {} entry in {feature_part}; try `{}`",
        slot.table_name(),
        suggested_invocation(name, slot, feature),
    )
}

/// Build the `pixi remove ...` invocation that targets a specific location.
pub(super) fn suggested_invocation(name: &str, slot: Slot, feature: &FeatureName) -> String {
    let mut parts = vec!["pixi".to_string(), "remove".to_string()];
    if let Some(flag) = slot.cli_flag() {
        parts.push(flag.to_string());
    }
    if !feature.is_default() {
        parts.push("--feature".to_string());
        parts.push(feature.to_string());
    }
    parts.push(name.to_string());
    parts.join(" ")
}

pub(super) fn is_similar(a: &str, b: &str) -> bool {
    a != b && strsim::jaro(a, b) > 0.85
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn parse(toml: &str) -> WorkspaceManifest {
        WorkspaceManifest::from_toml_str_with_base_dir(toml, Path::new(""))
            .expect("failed to parse manifest")
    }

    const MANIFEST: &str = r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64", "win-64"]

[dependencies]
numpy = "*"

[pypi-dependencies]
black = "*"

[target.linux-64.dependencies]
numpy = "*"
only-linux = "*"

[feature.dev.dependencies]
numpy = "*"
"#;

    #[test]
    fn locate_pypi_only() {
        let manifest = parse(MANIFEST);
        let locations = locate(&manifest, "black");
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].slot, Slot::Pypi);
        assert!(locations[0].feature.is_default());
        assert_eq!(locations[0].platforms, vec![None]);
    }

    #[test]
    fn locate_platform_specific_only() {
        // `only-linux` lives only in `[target.linux-64.dependencies]`.
        let manifest = parse(MANIFEST);
        let locations = locate(&manifest, "only-linux");
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].slot, Slot::Conda(SpecType::Run));
        assert_eq!(locations[0].platforms, vec![Some(Platform::Linux64)]);
    }

    #[test]
    fn locate_collapses_platforms_in_same_table() {
        // `numpy` is in both `[dependencies]` and
        // `[target.linux-64.dependencies]` of the default feature, plus the
        // `dev` feature: two distinct (feature, table) locations, with the
        // default-feature one spanning the agnostic and linux-64 scopes.
        let manifest = parse(MANIFEST);
        let locations = locate(&manifest, "numpy");
        assert_eq!(locations.len(), 2);

        let default = locations
            .iter()
            .find(|l| l.feature.is_default())
            .expect("default-feature location");
        assert_eq!(default.slot, Slot::Conda(SpecType::Run));
        assert_eq!(default.platforms, vec![None, Some(Platform::Linux64)]);

        let dev = locations
            .iter()
            .find(|l| !l.feature.is_default())
            .expect("dev-feature location");
        assert_eq!(dev.platforms, vec![None]);
    }

    #[test]
    fn locate_missing_is_empty() {
        let manifest = parse(MANIFEST);
        assert!(locate(&manifest, "does-not-exist").is_empty());
    }
}
