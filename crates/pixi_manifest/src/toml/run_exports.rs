use pixi_spec::PixiSpec;
use pixi_spec_containers::DependencyMap;
use rattler_conda_types::PackageName;
use toml_span::{DeserError, Value, de_helpers::TableHelper};

use crate::{
    PackageRunExports, TomlError,
    utils::{PixiSpanned, package_map::UniquePackageMap},
};

/// TOML representation of `[run-exports]` (or `[<...>.run-exports]`).
///
/// Each bucket maps `package-name` to a `PixiSpec`, mirroring the dependency
/// tables that already live next to it.
#[derive(Default, Debug)]
pub struct TomlRunExports {
    pub weak: Option<PixiSpanned<UniquePackageMap>>,
    pub strong: Option<PixiSpanned<UniquePackageMap>>,
    pub noarch: Option<PixiSpanned<UniquePackageMap>>,
    pub weak_constraints: Option<PixiSpanned<UniquePackageMap>>,
    pub strong_constraints: Option<PixiSpanned<UniquePackageMap>>,
}

impl TomlRunExports {
    /// Returns `true` if no buckets were specified.
    pub fn is_empty(&self) -> bool {
        self.weak.is_none()
            && self.strong.is_none()
            && self.noarch.is_none()
            && self.weak_constraints.is_none()
            && self.strong_constraints.is_none()
    }

    /// Convert this TOML representation into the in-memory data model,
    /// applying the pixi-build gate to source specs (same as other dependency
    /// tables).
    pub fn into_package_run_exports(
        self,
        is_pixi_build_enabled: bool,
    ) -> Result<PackageRunExports, TomlError> {
        fn lift(
            bucket: Option<PixiSpanned<UniquePackageMap>>,
            is_pixi_build_enabled: bool,
        ) -> Result<DependencyMap<PackageName, PixiSpec>, TomlError> {
            match bucket {
                Some(map) => Ok(map
                    .value
                    .into_inner(is_pixi_build_enabled)?
                    .into_iter()
                    .collect()),
                None => Ok(DependencyMap::default()),
            }
        }

        Ok(PackageRunExports {
            weak: lift(self.weak, is_pixi_build_enabled)?,
            strong: lift(self.strong, is_pixi_build_enabled)?,
            noarch: lift(self.noarch, is_pixi_build_enabled)?,
            weak_constraints: lift(self.weak_constraints, is_pixi_build_enabled)?,
            strong_constraints: lift(self.strong_constraints, is_pixi_build_enabled)?,
        })
    }
}

impl<'de> toml_span::Deserialize<'de> for TomlRunExports {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let mut th = TableHelper::new(value)?;
        let weak = th.optional("weak");
        let strong = th.optional("strong");
        let noarch = th.optional("noarch");
        let weak_constraints = th.optional("weak-constraints");
        let strong_constraints = th.optional("strong-constraints");
        th.finalize(None)?;
        Ok(TomlRunExports {
            weak,
            strong,
            noarch,
            weak_constraints,
            strong_constraints,
        })
    }
}
