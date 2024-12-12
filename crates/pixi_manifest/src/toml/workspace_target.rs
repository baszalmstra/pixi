use std::collections::HashMap;

use indexmap::IndexMap;
use pixi_spec::PixiSpec;
use serde::Deserialize;
use serde_with::serde_as;

use crate::{
    pypi::PyPiPackageName,
    utils::{package_map::UniquePackageMap, PixiSpanned},
    Activation, KnownPreviewFeature, Preview, PyPiRequirement, SpecType, Task, TaskName, TomlError,
    WorkspaceTarget,
};

#[serde_as]
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
pub struct TomlWorkspaceTarget {
    #[serde(default)]
    pub dependencies: Option<PixiSpanned<UniquePackageMap>>,

    #[serde(default)]
    pub host_dependencies: Option<PixiSpanned<UniquePackageMap>>,

    #[serde(default)]
    pub build_dependencies: Option<PixiSpanned<UniquePackageMap>>,

    #[serde(default)]
    pub pypi_dependencies: Option<IndexMap<PyPiPackageName, PyPiRequirement>>,

    /// Additional information to activate an environment.
    #[serde(default)]
    pub activation: Option<Activation>,

    /// Target specific tasks to run in the environment
    #[serde(default)]
    pub tasks: HashMap<TaskName, Task>,
}

impl TomlWorkspaceTarget {
    /// Called to convert this instance into a workspace target of a feature.
    pub fn into_workspace_target(self, preview: &Preview) -> Result<WorkspaceTarget, TomlError> {
        let pixi_build_enabled = preview.is_enabled(KnownPreviewFeature::PixiBuild);

        if pixi_build_enabled {
            if let Some(host_dependencies) = self.host_dependencies {
                return Err(TomlError::Generic(
                    "[host-dependencies] in features are not supported when `pixi-build` is enabled."
                        .into(),
                    host_dependencies.span,
                ));
            }

            if let Some(build_dependencies) = self.build_dependencies {
                return Err(TomlError::Generic(
                    "[build-dependencies] in features are not supported when `pixi-build` is enabled."
                        .into(),
                    build_dependencies.span,
                ));
            }
        }

        Ok(WorkspaceTarget {
            dependencies: combine_target_dependencies([
                (SpecType::Run, self.dependencies),
                (SpecType::Host, self.host_dependencies),
                (SpecType::Build, self.build_dependencies),
            ]),
            pypi_dependencies: self.pypi_dependencies,
            activation: self.activation,
            tasks: self.tasks,
        })
    }
}

/// Combines different target dependencies into a single map.
pub(super) fn combine_target_dependencies(
    iter: impl IntoIterator<Item = (SpecType, Option<PixiSpanned<UniquePackageMap>>)>,
) -> HashMap<SpecType, IndexMap<rattler_conda_types::PackageName, PixiSpec>> {
    iter.into_iter()
        .filter_map(|(ty, deps)| deps.map(|deps| (ty, deps.value.into())))
        .collect()
}
