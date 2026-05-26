use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use itertools::Itertools;
use pixi_record::{PixiRecord, SourceRecord};
use pixi_spec::{BinarySpec, ResolvedExcludeNewer, SourceLocationSpec, SourceSpec};
use pixi_spec_containers::DependencyMap;
use rattler_conda_types::{
    Channel, ChannelUrl, GenericVirtualPackage, MatchSpec, NamelessMatchSpec, PackageName,
    ParseMatchSpecOptions, Platform, RepoDataRecord, Version,
    package::{ArchiveIdentifier, CondaArchiveType, DistArchiveIdentifier},
};
use rattler_repodata_gateway::RepoData;
use rattler_solve::{ChannelPriority, SolveStrategy, SolverImpl};
use tokio::task::JoinError;
use url::Url;

/// Synthetic channel under which all source records are placed. A
/// matching channel pin gets injected into every matchspec built from a
/// `SourceSpec`, so a binary spec carrying a real channel cannot be
/// silently satisfied by a record that was meant to be a source build —
/// the solver surfaces the mismatch as an unsatisfiable conflict.
///
/// The `.invalid` TLD (RFC 2606) guarantees no real channel can collide.
const SOURCE_CHANNEL_URL: &str = "https://pixi-source.invalid/";

/// Synthetic channel object used to tag source records and source-spec
/// matchspecs. Built once from a URL; round-tripping through a depend
/// string and rattler's default `ChannelConfig` lands on the same
/// canonical name, so the solver matches our records to our matchspecs.
fn source_channel() -> std::sync::Arc<Channel> {
    use std::sync::OnceLock;
    static SOURCE_CHANNEL: OnceLock<std::sync::Arc<Channel>> = OnceLock::new();
    SOURCE_CHANNEL
        .get_or_init(|| {
            let url = Url::parse(SOURCE_CHANNEL_URL).expect("source channel URL is valid");
            std::sync::Arc::new(Channel::from_url(url))
        })
        .clone()
}

/// Attach the synthetic source channel to a nameless matchspec.
fn with_source_channel(mut spec: NamelessMatchSpec) -> NamelessMatchSpec {
    spec.channel = Some(source_channel());
    spec
}

/// For each depend whose name appears in `sources`, re-emit it with the
/// synthetic source channel pinned so that only a record from that
/// channel can satisfy it. Uses the fast name-only scan first to skip
/// the full matchspec parse for the common case (no source override).
fn rewrite_source_depends(
    depends: &[String],
    sources: &std::collections::BTreeMap<String, SourceLocationSpec>,
) -> Vec<String> {
    depends
        .iter()
        .map(|dep| {
            let name = PackageName::normalized_name_from_matchspec_str(dep);
            if !sources.contains_key(name.as_ref()) {
                return dep.clone();
            }
            let Ok(spec) = MatchSpec::from_str(dep, ParseMatchSpecOptions::lenient()) else {
                return dep.clone();
            };
            let (name, nameless) = spec.into_nameless();
            MatchSpec::from_nameless(with_source_channel(nameless), name).to_string()
        })
        .collect()
}

use crate::SourceMetadata;

/// Contains all information that describes the input of a conda environment.
/// All information about both binary and source packages is stored in the
/// specification, when solving this information is passed to the solver,
/// and the result is returned.
///
/// Unlike the higher-level pixi-environment solve, a
/// `SolveCondaEnvironmentSpec` carries everything the solver needs
/// already: source metadata has been resolved upstream, so this solve
/// is a leaf with no recursive calls.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct SolveCondaEnvironmentSpec {
    /// A name, useful for debugging purposes.
    pub name: Option<String>,

    /// Requirements on source packages.
    #[serde(skip_serializing_if = "DependencyMap::is_empty")]
    pub source_specs: DependencyMap<rattler_conda_types::PackageName, SourceSpec>,

    /// Requirements on binary packages.
    #[serde(skip_serializing_if = "DependencyMap::is_empty")]
    pub binary_specs: DependencyMap<rattler_conda_types::PackageName, BinarySpec>,

    /// Additional constraints of the environment
    #[serde(skip_serializing_if = "DependencyMap::is_empty")]
    pub constraints: DependencyMap<rattler_conda_types::PackageName, BinarySpec>,

    /// Dev source records whose dependencies should be installed.
    #[serde(skip)]
    pub dev_source_records: Vec<pixi_record::DevSourceRecord>,

    /// Available source repodata records.
    #[serde(skip)]
    pub source_repodata: Vec<Arc<SourceMetadata>>,

    /// Available Binary repodata records.
    #[serde(skip)]
    pub binary_repodata: Vec<RepoData>,

    /// The records of the packages that are currently already installed. These
    /// are used as hints to reduce the difference between individual solves.
    #[serde(skip)]
    pub installed: Vec<PixiRecord>,

    /// The platform to solve for
    pub platform: Platform,

    /// The channels to use for solving
    pub channels: Vec<ChannelUrl>,

    /// The virtual packages to include in the solve
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub virtual_packages: Vec<GenericVirtualPackage>,

    /// The strategy to use for solving
    #[serde(skip_serializing_if = "crate::is_default")]
    pub strategy: SolveStrategy,

    /// The priority of channels to use for solving
    #[serde(skip_serializing_if = "crate::is_default")]
    pub channel_priority: ChannelPriority,

    /// Exclude packages newer than the configured default and per-channel cutoffs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_newer: Option<ResolvedExcludeNewer>,
}

impl Default for SolveCondaEnvironmentSpec {
    fn default() -> Self {
        Self {
            name: None,
            source_specs: DependencyMap::default(),
            binary_specs: DependencyMap::default(),
            constraints: DependencyMap::default(),
            dev_source_records: vec![],
            source_repodata: vec![],
            binary_repodata: vec![],
            installed: vec![],
            platform: Platform::current(),
            channels: vec![],
            virtual_packages: vec![],
            strategy: SolveStrategy::default(),
            channel_priority: ChannelPriority::default(),
            exclude_newer: None,
        }
    }
}

impl SolveCondaEnvironmentSpec {
    /// Run the conda solve on a blocking thread. Caller provides
    /// `channel_config` so this function is reachable without a
    /// `CommandDispatcher` handle; the `ctx.solve_conda` ext method
    /// on [`pixi_compute_engine::ComputeCtx`] is the only caller.
    pub async fn solve_on_blocking_pool(
        self,
        channel_config: std::sync::Arc<rattler_conda_types::ChannelConfig>,
    ) -> Result<Vec<PixiRecord>, SolveCondaBlockingError> {
        // Solving is a CPU-intensive task, we spawn this on a background task to allow
        // for more concurrency.
        let solve_result = tokio::task::spawn_blocking(move || {
            let exclude_newer = self.exclude_newer.clone().map(Into::into);

            // Determine for which records we have source records because those records should only
            //  be installed as source records.
            let package_names_from_source = self
                .source_repodata
                .iter()
                .flat_map(|metadata| &metadata.records)
                .map(|metadata| &metadata.package_record().name)
                .dedup()
                .collect::<HashSet<_>>();

            // Filter all installed packages
            let installed: Vec<rattler_conda_types::RepoDataRecord> = self
                .installed
                .into_iter()
                // Only lock binary records
                .filter_map(|record| record.into_binary())
                // Filter any record we want as a source record
                .filter(|record| !package_names_from_source.contains(&record.package_record.name))
                .map(Arc::unwrap_or_clone)
                .collect();

            // Create direct dependencies on the source packages to feed to the solver.
            // Pin to the synthetic source channel so a binary spec for the same name
            // cannot silently satisfy it.
            let source_match_specs = self
                .source_specs
                .into_specs()
                .map(|(name, spec)| {
                    MatchSpec::from_nameless(
                        with_source_channel(spec.to_nameless_match_spec()),
                        name.into(),
                    )
                })
                .collect::<Vec<_>>();

            let binary_match_specs = self
                .binary_specs
                .into_match_specs(&channel_config)
                .map_err(SolveCondaEnvironmentError::SpecConversionError)?;

            let constrains_match_specs = self
                .constraints
                .into_match_specs(&channel_config)
                .map_err(SolveCondaEnvironmentError::SpecConversionError)?;

            // Create match specs for dev source packages themselves
            // Use a special prefix to avoid name clashes with real packages
            // When multiple variants exist for the same package, we only create one match spec
            // and let the solver choose which variant to use based on the constraints.
            // TODO: It would be nicer if the rattler solver could handle this directly
            // by introducing a special type of name/package for these virtual dependencies
            // that represent "install my dependencies but not me" packages.
            let dev_source_match_specs: Vec<_> = self
                .dev_source_records
                .iter()
                .map(|dev_source| dev_source.name.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .map(|name| {
                    let prefixed_name = format!("__pixi_dev_source_{}", name.as_normalized());
                    MatchSpec {
                        name: rattler_conda_types::PackageName::new_unchecked(prefixed_name).into(),
                        ..MatchSpec::default()
                    }
                })
                .collect();

            // Construct repodata records for source records and dev sources so that we can feed them to the
            // solver.
            let mut url_to_source_package = HashMap::new();
            let mut url_to_dev_source = HashMap::new();

            // Add source records
            let source_channel_canonical = source_channel().canonical_name();
            for source_metadata in &self.source_repodata {
                for record in &source_metadata.records {
                    let url = unique_url(record);
                    // Rewrite source-typed run-deps so the solver only accepts a
                    // matching source record (not a same-name binary candidate).
                    let mut package_record = record.data.package_record.clone();
                    package_record.depends =
                        rewrite_source_depends(&package_record.depends, &record.data.sources);
                    let repodata_record = RepoDataRecord {
                        package_record,
                        url: url.clone(),
                        identifier: DistArchiveIdentifier {
                            identifier: ArchiveIdentifier {
                                name: record.package_record().name.as_normalized().to_string(),
                                version: record.package_record().version.to_string(),
                                build_string: format!("{}_source", record.package_record().build),
                            },
                            archive_type: CondaArchiveType::Conda.into(),
                        },
                        channel: Some(source_channel_canonical.clone()),
                    };
                    let mut record = SourceRecord::clone(record);
                    record.build_source = source_metadata.source.build_source().cloned();
                    url_to_source_package.insert(url, (record, repodata_record));
                }
            }

            // Collect all dev source names for filtering
            let dev_source_names: std::collections::HashSet<_> = self
                .dev_source_records
                .iter()
                .map(|ds| ds.name.clone())
                .collect();

            // Add dev source records
            for dev_source in &self.dev_source_records {
                let url = unique_dev_source_url(dev_source);
                let prefixed_name =
                    format!("__pixi_dev_source_{}", dev_source.name.as_normalized());
                let build_string = dev_source_build_string(dev_source);
                let repodata_record = RepoDataRecord {
                    package_record: rattler_conda_types::PackageRecord {
                        subdir: self.platform.to_string(),
                        depends: dev_source
                            .dependencies
                            .iter_specs()
                            .filter(|(name, _)| !dev_source_names.contains(*name))
                            .map(|(name, spec)| {
                                let nameless = spec
                                    .clone()
                                    .try_into_nameless_match_spec_ref(&channel_config)
                                    .unwrap_or_default();
                                MatchSpec::from_nameless(nameless, name.clone().into()).to_string()
                            })
                            .collect(),
                        constrains: dev_source
                            .constraints
                            .iter_specs()
                            .filter(|(name, _)| !dev_source_names.contains(*name))
                            .filter_map(|(name, spec)| {
                                let nameless = spec
                                    .clone()
                                    .try_into_nameless_match_spec(&channel_config)
                                    .ok()?;
                                Some(
                                    MatchSpec::from_nameless(nameless, name.clone().into())
                                        .to_string(),
                                )
                            })
                            .collect(),
                        ..rattler_conda_types::PackageRecord::new(
                            rattler_conda_types::PackageName::new_unchecked(prefixed_name.clone()),
                            Version::major(0),
                            build_string.clone(),
                        )
                    },
                    url: url.clone(),
                    identifier: DistArchiveIdentifier {
                        identifier: ArchiveIdentifier {
                            name: prefixed_name.clone(),
                            version: "0".to_string(),
                            build_string: format!("{build_string}_devsource"),
                        },
                        archive_type: CondaArchiveType::Conda.into(),
                    },
                    channel: None,
                };
                url_to_dev_source.insert(url, (dev_source, repodata_record));
            }

            // Collect repodata records from the remote servers, source metadata, and dev sources
            // together. The source and dev source records go into the first "channel" to ensure
            // they are picked first.
            //
            // TODO: This only holds up when the channel priority is strict. We should
            // probably enforce this better somehow..
            let mut solvable_records = Vec::with_capacity(self.binary_repodata.len() + 1);
            solvable_records.push(
                url_to_source_package
                    .values()
                    .map(|(_, record)| record)
                    .chain(url_to_dev_source.values().map(|(_, record)| record))
                    .collect_vec(),
            );
            for repo_data in &self.binary_repodata {
                solvable_records.push(repo_data.iter().collect_vec());
            }

            // Construct a solver task that we can start solving.
            let task = rattler_solve::SolverTask {
                specs: source_match_specs
                    .into_iter()
                    .chain(binary_match_specs)
                    .chain(dev_source_match_specs)
                    .collect(),
                locked_packages: installed.iter().collect(),
                virtual_packages: self.virtual_packages,
                channel_priority: self.channel_priority,
                exclude_newer,
                strategy: self.strategy,
                constraints: constrains_match_specs,
                ..rattler_solve::SolverTask::from_iter(solvable_records)
            };

            let solver_result = rattler_solve::resolvo::Solver.solve(task)?;

            // Convert the results back into pixi records.
            Ok::<_, SolveCondaEnvironmentError>(
                solver_result
                    .records
                    .into_iter()
                    .filter_map(|record| {
                        if let Some(source_record) = url_to_source_package.remove(&record.url) {
                            // This is a source package, we want to return the source record
                            // instead of the binary record.
                            return Some(PixiRecord::Source(Arc::new(source_record.0)));
                        } else if let Some(_dev_source) = url_to_dev_source.remove(&record.url) {
                            // This is a dev source, we don't want to return it.
                            return None;
                        }

                        Some(PixiRecord::Binary(Arc::new(record)))
                    })
                    .collect_vec(),
            )
        })
        .await;

        // Error out if the background task failed or was canceled.
        match solve_result.map_err(JoinError::try_into_panic) {
            Err(Err(_)) => Err(SolveCondaBlockingError::JoinCancelled),
            Err(Ok(panic)) => Err(SolveCondaBlockingError::Panic(panic)),
            Ok(Err(err)) => Err(SolveCondaBlockingError::Solve(err)),
            Ok(Ok(result)) => Ok(result),
        }
    }
}

/// Error kinds returned by `SolveCondaEnvironmentSpec::solve_on_blocking_pool`.
/// The outer `SolveCondaEnvironmentSpec::solve` maps these onto
/// [`crate::CommandDispatcherError`] for backwards compatibility; new
/// compute-engine call sites handle these directly.
#[derive(Debug)]
pub enum SolveCondaBlockingError {
    /// The solve itself returned an error.
    Solve(SolveCondaEnvironmentError),
    /// The blocking task was cancelled (runtime shutdown).
    JoinCancelled,
    /// The blocking task panicked; payload is passed through so callers
    /// can decide whether to resume the unwind.
    Panic(Box<dyn std::any::Any + Send + 'static>),
}

/// Generates a unique URL for a source record.
fn unique_url(source: &SourceRecord) -> Url {
    let mut url = source.manifest_source().identifiable_url();

    // Add unique identifiers to the URL.
    url.query_pairs_mut()
        .append_pair("name", source.package_record().name.as_source())
        .append_pair("version", &source.package_record().version.as_str())
        .append_pair("build", &source.package_record().build)
        .append_pair("subdir", &source.package_record().subdir);

    url
}

/// Generates a unique URL for a dev source record.
fn unique_dev_source_url(dev_source: &pixi_record::DevSourceRecord) -> Url {
    let mut url = dev_source.source.identifiable_url();

    // Add unique identifiers to the URL.
    let mut pairs = url.query_pairs_mut();
    pairs.append_pair("name", dev_source.name.as_source());

    for (key, value) in &dev_source.variants {
        pairs.append_pair(&format!("_{key}"), value.to_string().as_str());
    }

    drop(pairs);

    url
}

/// Generates a unique build string for a dev source record based on its variants.
/// Uses a hash of the variants to ensure uniqueness when multiple variants exist.
fn dev_source_build_string(dev_source: &pixi_record::DevSourceRecord) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Hash the variants to create a stable, unique build string
    let mut hasher = DefaultHasher::new();
    dev_source.variants.hash(&mut hasher);
    let hash = hasher.finish();
    format!("{hash:x}")
}

#[derive(Debug, thiserror::Error)]
pub enum SolveCondaEnvironmentError {
    #[error(transparent)]
    SolveError(#[from] rattler_solve::SolveError),

    #[error(transparent)]
    SpecConversionError(#[from] pixi_spec::SpecConversionError),

    /// Surfaced when the binary repodata fetch performed inside
    /// [`SolveCondaKey`](crate::keys::SolveCondaKey)'s compute body
    /// fails.
    #[error(transparent)]
    Gateway(#[from] rattler_repodata_gateway::GatewayError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use pixi_spec::{PathSourceSpec, SourceLocationSpec};
    use rattler_conda_types::NamelessMatchSpec;

    use super::*;

    #[test]
    fn source_channel_canonical_name_is_stable() {
        // The matchspec serializer emits the channel via `channel.name()`,
        // which for our URL-based channel returns the base URL string.
        // The solver re-parses that through the default `ChannelConfig`,
        // and the resulting channel must canonicalize to the same string
        // as the one we attached.
        let chan = source_channel();
        let parsed = rattler_conda_types::Channel::from_str(
            chan.name(),
            &rattler_conda_types::ChannelConfig::default_with_root_dir(
                std::env::current_dir().unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(parsed.canonical_name(), chan.canonical_name());
    }

    #[test]
    fn with_source_channel_sets_the_synthetic_channel() {
        let tagged = with_source_channel(NamelessMatchSpec::default());
        let chan = tagged.channel.expect("channel should be set");
        assert_eq!(chan.canonical_name(), source_channel().canonical_name());
    }

    #[test]
    fn rewrite_depends_pins_only_source_entries() {
        let mut sources: BTreeMap<String, SourceLocationSpec> = BTreeMap::new();
        sources.insert(
            "xgcm".to_string(),
            SourceLocationSpec::Path(PathSourceSpec {
                path: typed_path::Utf8TypedPathBuf::from("./xgcm"),
            }),
        );
        let depends = vec!["xgcm".to_string(), "numpy >=1.0".to_string()];
        let rewritten = rewrite_source_depends(&depends, &sources);

        // numpy is unchanged because it's not in the sources map.
        assert_eq!(rewritten[1], "numpy >=1.0");

        // xgcm gets the synthetic source channel.
        let spec = MatchSpec::from_str(&rewritten[0], ParseMatchSpecOptions::lenient()).unwrap();
        let chan = spec.channel.expect("xgcm should be channel-tagged");
        assert_eq!(chan.canonical_name(), source_channel().canonical_name());
    }

    /// Regression for https://github.com/prefix-dev/pixi/issues/6161.
    ///
    /// A source package's source-typed run-dep with its own version
    /// constraint must keep the constraint after rewriting and also
    /// carry the source channel pin, so a binary spec with a different
    /// channel cannot silently satisfy the same name.
    #[test]
    fn rewrite_preserves_version_and_pins_channel_for_versioned_source_dep() {
        let mut sources: BTreeMap<String, SourceLocationSpec> = BTreeMap::new();
        sources.insert(
            "xgcm".to_string(),
            SourceLocationSpec::Path(PathSourceSpec {
                path: typed_path::Utf8TypedPathBuf::from("./xgcm"),
            }),
        );
        let depends = vec!["xgcm >=0.9".to_string()];
        let rewritten = rewrite_source_depends(&depends, &sources);

        let spec = MatchSpec::from_str(&rewritten[0], ParseMatchSpecOptions::lenient()).unwrap();
        let source_chan = spec
            .channel
            .as_ref()
            .expect("channel should be pinned")
            .canonical_name();
        assert_eq!(source_chan, source_channel().canonical_name());
        assert!(
            spec.version.is_some(),
            "version constraint must survive the rewrite: {}",
            rewritten[0]
        );

        // A binary spec for the same name pointing at a different channel
        // must not have the same canonical name as the source channel —
        // this is the property that lets the solver surface the
        // conflict instead of silently picking the binary.
        let binary = MatchSpec::from_str(
            "xgcm[version=\"0.9.*\",channel=conda-forge]",
            ParseMatchSpecOptions::lenient(),
        )
        .unwrap();
        assert_ne!(
            binary.channel.unwrap().canonical_name(),
            source_chan,
        );
    }
}
