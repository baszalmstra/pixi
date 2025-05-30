use std::collections::HashSet;

use chrono::{DateTime, Utc};
use indexmap::IndexSet;
use miette::Diagnostic;
use rattler_conda_types::{
    ChannelConfig, ChannelUrl, NamedChannelOrUrl, ParseChannelError, Platform,
};

use crate::{
    CondaDependencies, PrioritizedChannel, PyPiDependencies, SpecType, SystemRequirements,
    has_features_iter::HasFeaturesIter, has_manifest_ref::HasWorkspaceManifest,
    pypi::pypi_options::PypiOptions, workspace::ChannelPriority,
};

/// ChannelPriorityCombination error, thrown when multiple channel priorities
/// are set
#[derive(Debug, thiserror::Error, Diagnostic)]
#[error("Multiple channel priorities are not allowed in a single environment")]
pub struct ChannelPriorityCombinationError;

/// A trait that implement various methods for collections that combine
/// attributes of Features It is implemented by Environment, GroupedEnvironment
/// and SolveGroup. Remove some of the boilerplate of combining features and its
/// derived data from multiple sources.
///
/// The name of the lifetime parameter is named `'source` that the borrow comes
/// from the source of the data for most implementations this will be the pixi
/// project.
///
/// There is blanket implementation available for all types that implement
/// [`HasWorkspaceManifest`] and [`HasFeaturesIter`]
pub trait FeaturesExt<'source>: HasWorkspaceManifest<'source> + HasFeaturesIter<'source> {
    /// Returns the channels associated with this collection.
    ///
    /// Users can specify custom channels on a per-feature basis. This method
    /// collects and deduplicates all the channels from all the features in
    /// the order they are defined in the manifest.
    ///
    /// If a feature does not specify any channel the default channels from the
    /// project metadata are used instead.
    fn channels(&self) -> IndexSet<&'source NamedChannelOrUrl> {
        // Collect all the channels from the features in one set,
        // deduplicate them and sort them on feature index, default feature comes last.
        let channels = self.features().flat_map(|feature| match &feature.channels {
            Some(channels) => channels,
            None => &self.workspace_manifest().workspace.channels,
        });

        PrioritizedChannel::sort_channels_by_priority(channels).collect()
    }

    /// Returns the channels associated with this collection.
    ///
    /// This function is similar to [`Self::channels]` but it resolves the
    /// channel urls using the provided channel config.
    fn channel_urls(
        &self,
        channel_config: &ChannelConfig,
    ) -> Result<Vec<ChannelUrl>, ParseChannelError> {
        self.channels()
            .into_iter()
            .cloned()
            .map(|channel| channel.into_base_url(channel_config))
            .collect()
    }

    /// Returns the channel priority, error on multiple values, return None if
    /// no value is set.
    ///
    /// When using multiple channel priorities over different features we should
    /// error as the user should decide what they want.
    fn channel_priority(&self) -> Result<Option<ChannelPriority>, ChannelPriorityCombinationError> {
        let mut channel_priority = None;
        for feature in self.features() {
            if let Some(priority) = feature.channel_priority {
                if channel_priority == Some(priority) {
                    return Err(ChannelPriorityCombinationError);
                }
                channel_priority = Some(priority);
            }
        }
        Ok(channel_priority)
    }

    /// Returns whether packages should be excluded newer than a certain date.
    fn exclude_newer(&self) -> Option<DateTime<Utc>> {
        self.workspace_manifest()
            .workspace
            .exclude_newer
            .map(Into::into)
    }

    /// Returns the strategy for solving packages.
    fn solve_strategy(&self) -> rattler_solve::SolveStrategy {
        rattler_solve::SolveStrategy::default()
    }

    /// Returns the platforms that this collection is compatible with.
    ///
    /// Which platforms a collection support depends on which platforms the
    /// selected features of the collection supports. The platforms that are
    /// supported by the collection is the intersection of the platforms
    /// supported by its features.
    ///
    /// Features can specify which platforms they support through the
    /// `platforms` key. If a feature does not specify any platforms the
    /// features defined by the project are used.
    fn platforms(&self) -> HashSet<Platform> {
        self.features()
            .map(|feature| {
                match &feature.platforms {
                    Some(platforms) => platforms,
                    None => &self.workspace_manifest().workspace.platforms,
                }
                .iter()
                .copied()
                .collect::<HashSet<_>>()
            })
            .reduce(|accumulated_platforms, feat| {
                accumulated_platforms.intersection(&feat).copied().collect()
            })
            .unwrap_or_default()
    }

    /// Returns the system requirements for this collection.
    ///
    /// The system requirements of the collection are the union of the system
    /// requirements of all the features in the collection. If multiple
    /// features specify a requirement for the same system package, the
    /// highest is chosen.
    fn local_system_requirements(&self) -> SystemRequirements {
        self.features()
            .map(|feature| &feature.system_requirements)
            .fold(SystemRequirements::default(), |acc, req| {
                acc.union(req)
                    .expect("system requirements should have been validated upfront")
            })
    }

    /// Returns true if any of the features has any reference to a pypi
    /// dependency.
    fn has_pypi_dependencies(&self) -> bool {
        self.features().any(|f| f.has_pypi_dependencies())
    }

    /// Returns the PyPi dependencies to install for this collection.
    ///
    /// The dependencies of all features are combined. This means that if two
    /// features define a requirement for the same package that both
    /// requirements are returned. The different requirements per package
    /// are sorted in the same order as the features they came from.
    fn pypi_dependencies(&self, platform: Option<Platform>) -> PyPiDependencies {
        self.features()
            .filter_map(|f| f.pypi_dependencies(platform))
            .into()
    }

    /// Returns the dependencies to install for this collection.
    ///
    /// The dependencies of all features are combined. This means that if two
    /// features define a requirement for the same package that both
    /// requirements are returned. The different requirements per package
    /// are sorted in the same order as the features they came from.
    ///
    /// If the `platform` is `None` no platform specific dependencies are taken
    /// into consideration.
    fn dependencies(&self, kind: SpecType, platform: Option<Platform>) -> CondaDependencies {
        self.features()
            .filter_map(|f| f.dependencies(kind, platform))
            .into()
    }

    /// Returns the combined dependencies to install for this collection.
    ///
    /// The `build` dependencies overwrite the `host` dependencies which
    /// overwrite the `run` dependencies.
    ///
    /// The dependencies of all features are combined. This means that if two
    /// features define a requirement for the same package that both
    /// requirements are returned. The different requirements per package
    /// are sorted in the same order as the features they came from.
    ///
    /// If the `platform` is `None` no platform specific dependencies are taken
    /// into consideration.
    fn combined_dependencies(&self, platform: Option<Platform>) -> CondaDependencies {
        self.features()
            .filter_map(|f| f.combined_dependencies(platform))
            .into()
    }

    /// Returns the pypi options for this collection.
    ///
    /// The pypi options of all features are combined. They will be combined in
    /// the order that they are defined in the manifest.
    /// The index-url is a special case and can only be defined once. This
    /// should have been verified beforehand.
    fn pypi_options(&self) -> PypiOptions {
        // Collect all the pypi-options from the features in one set,
        // deduplicate them and sort them on feature index, default feature comes last.
        let pypi_options: Vec<_> = self
            .features()
            .filter_map(|feature| {
                if feature.pypi_options().is_none() {
                    self.workspace_manifest().workspace.pypi_options.as_ref()
                } else {
                    feature.pypi_options()
                }
            })
            .collect();

        // Merge all the pypi options into one.
        pypi_options
            .into_iter()
            .fold(PypiOptions::default(), |acc, opts| {
                acc.union(opts)
                    .expect("merging of pypi-options should already have been checked")
            })
    }
}

impl<'source, FeatureCollection> FeaturesExt<'source> for FeatureCollection where
    FeatureCollection: HasWorkspaceManifest<'source> + HasFeaturesIter<'source>
{
}
