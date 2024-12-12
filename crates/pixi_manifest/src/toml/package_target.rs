use serde::Deserialize;
use serde_with::serde_as;

use crate::{
    target::PackageTarget,
    utils::{package_map::UniquePackageMap, PixiSpanned},
    SpecType, TomlError,
};

#[serde_as]
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct TomlPackageTarget {
    #[serde(default)]
    pub run_dependencies: Option<PixiSpanned<UniquePackageMap>>,

    #[serde(default)]
    pub host_dependencies: Option<PixiSpanned<UniquePackageMap>>,

    #[serde(default)]
    pub build_dependencies: Option<PixiSpanned<UniquePackageMap>>,
}

impl TomlPackageTarget {
    /// Called to convert this instance into a workspace target of a feature.
    pub fn into_package_target(self) -> Result<PackageTarget, TomlError> {
        Ok(PackageTarget {
            dependencies: [
                (SpecType::Run, self.run_dependencies),
                (SpecType::Host, self.host_dependencies),
                (SpecType::Build, self.build_dependencies),
            ]
            .into_iter()
            .filter_map(|(ty, deps)| deps.map(|deps| (ty, deps.value.into())))
            .collect(),
        })
    }
}
