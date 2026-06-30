//! `[package.run-exports.*]`: declares that a package's dependents should
//! automatically receive certain dependencies (the conda "run-exports"
//! mechanism). Mirrors `CondaOutputRunExports`'s field naming
//! (`strong-constrains`/`weak-constrains`, kebab-case) and pixi_manifest's
//! established kebab-case convention.

use pixi_spec_containers::DependencyMap;
use rattler_conda_types::PackageName;

use crate::package_dependency_spec::PackageDependencySpec;

/// The fully-resolved `[package.run-exports.*]` declarations for a single
/// (unconditional or conditional) package target.
#[derive(Default, Debug, Clone)]
pub struct ManifestRunExports {
    /// `[package.run-exports.noarch]`: applied only to noarch packages.
    pub noarch: DependencyMap<PackageName, PackageDependencySpec>,
    /// `[package.run-exports.strong]`: applied from build and host env to run env.
    pub strong: DependencyMap<PackageName, PackageDependencySpec>,
    /// `[package.run-exports.weak]`: applied from host env to run env.
    pub weak: DependencyMap<PackageName, PackageDependencySpec>,
    /// `[package.run-exports.strong-constrains]`: strong run-constrains.
    pub strong_constrains: DependencyMap<PackageName, PackageDependencySpec>,
    /// `[package.run-exports.weak-constrains]`: weak run-constrains.
    pub weak_constrains: DependencyMap<PackageName, PackageDependencySpec>,
}

impl ManifestRunExports {
    /// Returns `true` if none of the five buckets contain any entries.
    pub fn is_empty(&self) -> bool {
        self.noarch.is_empty()
            && self.strong.is_empty()
            && self.weak.is_empty()
            && self.strong_constrains.is_empty()
            && self.weak_constrains.is_empty()
    }
}
