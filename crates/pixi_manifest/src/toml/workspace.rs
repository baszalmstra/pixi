use std::{collections::HashMap, path::PathBuf};

use indexmap::{IndexMap, IndexSet};
use pixi_toml::{TomlFromStr, TomlHashMap, TomlIndexMap, TomlIndexSet, TomlWith};
use rattler_conda_types::{NamedChannelOrUrl, Platform, Version};
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr};
use thiserror::Error;
use toml_span::{de_helpers::TableHelper, DeserError, Value};
use url::Url;

use crate::{
    preview::Preview, pypi::pypi_options::PypiOptions, toml::platform::TomlPlatform,
    utils::PixiSpanned, workspace::ChannelPriority, PrioritizedChannel, TargetSelector, Targets,
    Workspace,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct TomlWorkspaceTarget {
    build_variants: Option<HashMap<String, Vec<String>>>,
}

/// The TOML representation of the `[[workspace]]` section in a pixi manifest.
#[serde_as]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct TomlWorkspace {
    // In TOML the workspace name can be empty. It is a required field though, but this is enforced
    // when converting the TOML model to the actual manifest. When using a PyProject we want to use
    // the name from the PyProject file.
    pub name: Option<String>,

    #[serde_as(as = "Option<DisplayFromStr>")]
    pub version: Option<Version>,
    pub description: Option<String>,
    pub authors: Option<Vec<String>>,
    #[serde_as(as = "IndexSet<super::TomlPrioritizedChannel>")]
    pub channels: IndexSet<PrioritizedChannel>,
    #[serde(default)]
    pub channel_priority: Option<ChannelPriority>,
    // TODO: This is actually slightly different from the rattler_conda_types::Platform because it
    //     should not include noarch.
    pub platforms: PixiSpanned<IndexSet<Platform>>,
    pub license: Option<String>,
    pub license_file: Option<PathBuf>,
    pub readme: Option<PathBuf>,
    pub homepage: Option<Url>,
    pub repository: Option<Url>,
    pub documentation: Option<Url>,
    pub conda_pypi_map: Option<HashMap<NamedChannelOrUrl, String>>,
    pub pypi_options: Option<PypiOptions>,

    #[serde(default)]
    pub preview: Preview,

    #[serde(default)]
    pub target: IndexMap<PixiSpanned<TargetSelector>, TomlWorkspaceTarget>,

    pub build_variants: Option<HashMap<String, Vec<String>>>,
}

/// Defines some of the properties that might be defined in other parts of the
/// manifest but we do require to be set in the workspace section.
///
/// This can be used to inject these properties.
#[derive(Debug, Clone, Default)]
pub struct ExternalWorkspaceProperties {
    pub name: Option<String>,
    pub version: Option<Version>,
    pub description: Option<String>,
    pub authors: Option<Vec<String>>,
    pub license: Option<String>,
    pub license_file: Option<PathBuf>,
    pub readme: Option<PathBuf>,
    pub homepage: Option<Url>,
    pub repository: Option<Url>,
    pub documentation: Option<Url>,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("missing `name` in `[workspace]` section")]
    MissingName,
}

impl TomlWorkspace {
    pub fn into_workspace(
        self,
        external: ExternalWorkspaceProperties,
    ) -> Result<Workspace, WorkspaceError> {
        Ok(Workspace {
            name: self
                .name
                .or(external.name)
                .ok_or(WorkspaceError::MissingName)?,
            version: self.version.or(external.version),
            description: self.description.or(external.description),
            authors: self.authors.or(external.authors),
            license: self.license.or(external.license),
            license_file: self.license_file.or(external.license_file),
            readme: self.readme.or(external.readme),
            homepage: self.homepage.or(external.homepage),
            repository: self.repository.or(external.repository),
            documentation: self.documentation.or(external.documentation),
            channels: self.channels,
            channel_priority: self.channel_priority,
            platforms: self.platforms,
            conda_pypi_map: self.conda_pypi_map,
            pypi_options: self.pypi_options,
            preview: self.preview,
            build_variants: Targets::from_default_and_user_defined(
                self.build_variants,
                self.target
                    .clone()
                    .into_iter()
                    .map(|(k, v)| (k, v.build_variants))
                    .collect(),
            ),
        })
    }
}

impl<'de> toml_span::Deserialize<'de> for TomlWorkspace {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let mut th = TableHelper::new(value)?;

        let name = th.optional("name");
        let version = th
            .optional::<TomlFromStr<_>>("version")
            .map(TomlFromStr::into_inner);
        let description = th.optional("description");
        let authors = th.optional("authors");
        let channels = th
            .required::<TomlIndexSet<_>>("channels")
            .map(TomlIndexSet::into_inner)?;
        let channel_priority = th.optional("channel-priority");
        let platforms = th
            .optional::<TomlWith<_, PixiSpanned<TomlIndexSet<TomlPlatform>>>>("platforms")
            .map(TomlWith::into_inner);
        let license = th.optional("license");
        let license_file = th
            .optional::<TomlFromStr<_>>("license-file")
            .map(TomlFromStr::into_inner);
        let readme = th
            .optional::<TomlFromStr<_>>("readme")
            .map(TomlFromStr::into_inner);
        let homepage = th
            .optional::<TomlFromStr<_>>("homepage")
            .map(TomlFromStr::into_inner);
        let repository = th
            .optional::<TomlFromStr<_>>("repository")
            .map(TomlFromStr::into_inner);
        let documentation = th
            .optional::<TomlFromStr<_>>("documentation")
            .map(TomlFromStr::into_inner);
        let conda_pypi_map = th
            .optional::<TomlHashMap<_, _>>("conda-pypi-map")
            .map(TomlHashMap::into_inner);
        let pypi_options = th.optional("pypi-options");
        let preview = th.optional("preview").unwrap_or_default();
        let target = th
            .optional::<TomlIndexMap<_, _>>("target")
            .map(TomlIndexMap::into_inner);
        let build_variants = th
            .optional::<TomlHashMap<_, _>>("build-variants")
            .map(TomlHashMap::into_inner);

        th.finalize(None)?;

        Ok(TomlWorkspace {
            name,
            version,
            description,
            authors,
            channels,
            channel_priority,
            platforms: platforms.unwrap_or_default(),
            license,
            license_file,
            readme,
            homepage,
            repository,
            documentation,
            conda_pypi_map,
            pypi_options,
            preview,
            target: target.unwrap_or_default(),
            build_variants,
        })
    }
}

impl<'de> toml_span::Deserialize<'de> for TomlWorkspaceTarget {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let mut th = TableHelper::new(value)?;

        let build_variants = th
            .optional::<TomlHashMap<_, _>>("build-variants")
            .map(TomlHashMap::into_inner);

        th.finalize(None)?;

        Ok(TomlWorkspaceTarget { build_variants })
    }
}
