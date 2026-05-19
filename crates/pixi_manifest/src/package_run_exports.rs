use pixi_spec::PixiSpec;
use pixi_spec_containers::DependencyMap;
use rattler_conda_types::PackageName;

/// Run-exports declared by a package, split into the five conda buckets.
///
/// Run-exports propagate dependencies to downstream consumers of the package:
/// see <https://rattler.build/latest/reference/recipe_file/#run-exports>.
#[derive(Default, Debug, Clone)]
pub struct PackageRunExports {
    /// Added to the consumer's `run` requirements when this package is a
    /// host-dependency of the consumer.
    pub weak: DependencyMap<PackageName, PixiSpec>,

    /// Added to the consumer's `run` requirements when this package is a
    /// build- or host-dependency of the consumer.
    pub strong: DependencyMap<PackageName, PixiSpec>,

    /// Applied only when the downstream consumer is a noarch package.
    pub noarch: DependencyMap<PackageName, PixiSpec>,

    /// Like `weak`, but contributes to the consumer's `run_constraints`
    /// instead of `run`.
    pub weak_constraints: DependencyMap<PackageName, PixiSpec>,

    /// Like `strong`, but contributes to the consumer's `run_constraints`
    /// instead of `run`.
    pub strong_constraints: DependencyMap<PackageName, PixiSpec>,
}

impl PackageRunExports {
    /// Returns `true` if every bucket is empty.
    pub fn is_empty(&self) -> bool {
        self.weak.is_empty()
            && self.strong.is_empty()
            && self.noarch.is_empty()
            && self.weak_constraints.is_empty()
            && self.strong_constraints.is_empty()
    }
}
