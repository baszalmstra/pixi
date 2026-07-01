//! `[package.run-exports.*]`: declares that a package's dependents should
//! automatically receive certain dependencies (the conda "run-exports"
//! mechanism). Mirrors `CondaOutputRunExports`'s field naming
//! (`strong-constrains`/`weak-constrains`, kebab-case) and pixi_manifest's
//! established kebab-case convention.

use pixi_spec_containers::DependencyMap;
use rattler_conda_types::PackageName;

use crate::package_dependency_spec::PackageDependencySpec;

/// The five run-export buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunExportKind {
    /// Applied only to noarch packages.
    Noarch,
    /// Applied from build and host env to run env.
    Strong,
    /// Applied from host env to run env.
    Weak,
    /// Strong run-constrains.
    StrongConstrains,
    /// Weak run-constrains.
    WeakConstrains,
}

impl RunExportKind {
    /// All kinds, in canonical (manifest/wire) order.
    pub const ALL: [RunExportKind; 5] = [
        RunExportKind::Noarch,
        RunExportKind::Strong,
        RunExportKind::Weak,
        RunExportKind::StrongConstrains,
        RunExportKind::WeakConstrains,
    ];

    /// TOML table key / user-facing name ("strong-constrains" etc.).
    pub fn name(self) -> &'static str {
        match self {
            RunExportKind::Noarch => "noarch",
            RunExportKind::Strong => "strong",
            RunExportKind::Weak => "weak",
            RunExportKind::StrongConstrains => "strong-constrains",
            RunExportKind::WeakConstrains => "weak-constrains",
        }
    }
}

/// One value per run-export kind.
#[derive(Debug, Clone, Default)]
pub struct RunExports<T> {
    /// `[package.run-exports.noarch]`: applied only to noarch packages.
    pub noarch: T,
    /// `[package.run-exports.strong]`: applied from build and host env to run env.
    pub strong: T,
    /// `[package.run-exports.weak]`: applied from host env to run env.
    pub weak: T,
    /// `[package.run-exports.strong-constrains]`: strong run-constrains.
    pub strong_constrains: T,
    /// `[package.run-exports.weak-constrains]`: weak run-constrains.
    pub weak_constrains: T,
}

impl<T> RunExports<T> {
    /// Returns the bucket for `kind`.
    pub fn get(&self, kind: RunExportKind) -> &T {
        match kind {
            RunExportKind::Noarch => &self.noarch,
            RunExportKind::Strong => &self.strong,
            RunExportKind::Weak => &self.weak,
            RunExportKind::StrongConstrains => &self.strong_constrains,
            RunExportKind::WeakConstrains => &self.weak_constrains,
        }
    }

    /// Returns the bucket for `kind`, mutably.
    pub fn get_mut(&mut self, kind: RunExportKind) -> &mut T {
        match kind {
            RunExportKind::Noarch => &mut self.noarch,
            RunExportKind::Strong => &mut self.strong,
            RunExportKind::Weak => &mut self.weak,
            RunExportKind::StrongConstrains => &mut self.strong_constrains,
            RunExportKind::WeakConstrains => &mut self.weak_constrains,
        }
    }

    /// Maps every bucket through `f`, short-circuiting on the first error.
    pub fn try_map<U, E>(
        self,
        mut f: impl FnMut(RunExportKind, T) -> Result<U, E>,
    ) -> Result<RunExports<U>, E> {
        Ok(RunExports {
            noarch: f(RunExportKind::Noarch, self.noarch)?,
            strong: f(RunExportKind::Strong, self.strong)?,
            weak: f(RunExportKind::Weak, self.weak)?,
            strong_constrains: f(RunExportKind::StrongConstrains, self.strong_constrains)?,
            weak_constrains: f(RunExportKind::WeakConstrains, self.weak_constrains)?,
        })
    }

    /// Iterates over the buckets in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = (RunExportKind, &T)> {
        RunExportKind::ALL
            .iter()
            .map(move |&kind| (kind, self.get(kind)))
    }
}

/// The fully-resolved `[package.run-exports.*]` declarations for a single
/// (unconditional or conditional) package target.
pub type ManifestRunExports = RunExports<DependencyMap<PackageName, PackageDependencySpec>>;

impl ManifestRunExports {
    /// Returns `true` if none of the five buckets contain any entries.
    pub fn is_empty(&self) -> bool {
        self.iter().all(|(_, bucket)| bucket.is_empty())
    }
}
