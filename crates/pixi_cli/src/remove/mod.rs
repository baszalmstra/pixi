mod error;
mod locate;

use std::str::FromStr;

use clap::Parser;
use indexmap::IndexMap;
use pixi_api::{
    WorkspaceContext,
    workspace::{DependencyOptions, Removal, RemoveError},
};
use pixi_config::ConfigCli;
use pixi_core::{DependencyType, Workspace, WorkspaceLocator};
use pixi_manifest::HasWorkspaceManifest;
use pixi_pypi_spec::PypiPackageName;
use rattler_conda_types::{PackageName, Platform};

use crate::{cli_config::LockFileUpdateConfig, has_specs::HasSpecs};
use crate::{
    cli_config::{DependencyConfig, NoInstallConfig, WorkspaceConfig},
    cli_interface::CliInterface,
};

use error::{AmbiguousRemovalError, DependencyRemovalError, Scope};
use locate::{Location, Slot, locate};

/// Removes dependencies from the workspace.
///
/// A bare `pixi remove <pkg>...` searches every dependency table — conda and
/// PyPI, every feature, every platform — and removes each package from wherever
/// it is defined. Passing a location flag (`--pypi`, `--host`, `--build`,
/// `--feature`, or `--platform`) instead restricts the removal to that table.
///
///  If the workspace manifest is a `pyproject.toml`, removing a pypi dependency
/// with the `--pypi` flag will remove it from either
///
/// - the native pyproject `project.dependencies` array or, if a feature is
///   specified, the native `project.optional-dependencies` table
///
/// - pixi `pypi-dependencies` tables of the default feature or, if a feature is
///   specified, a named feature
#[derive(Debug, Default, Parser)]
#[clap(arg_required_else_help = true)]
pub struct Args {
    #[clap(flatten)]
    pub config_source: pixi_config::ConfigSourceCli,

    #[clap(flatten)]
    pub workspace_config: WorkspaceConfig,

    #[clap(flatten)]
    pub dependency_config: DependencyConfig,

    #[clap(flatten)]
    pub no_install_config: NoInstallConfig,
    #[clap(flatten)]
    pub lock_file_update_config: LockFileUpdateConfig,

    #[clap(flatten)]
    pub config: ConfigCli,
}

impl TryFrom<&Args> for DependencyOptions {
    type Error = miette::Error;

    fn try_from(args: &Args) -> miette::Result<Self> {
        Ok(DependencyOptions {
            feature: args.dependency_config.feature.clone(),
            platforms: args.dependency_config.platforms.clone(),
            no_install: args.no_install_config.no_install,
            lock_file_usage: args.lock_file_update_config.lock_file_usage()?,
        })
    }
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let workspace = WorkspaceLocator::for_cli()
        .with_global_config_source(args.config_source.source())
        .with_search_start(args.workspace_config.workspace_locator_start())
        .locate()?
        .with_cli_config(args.config.clone());

    let workspace_ctx = WorkspaceContext::new(CliInterface {}, workspace.clone());

    // Without any location flag, search every table and remove each package
    // from wherever it is defined.
    if !location_specified(&args.dependency_config) {
        return execute_auto(&args, &workspace, &workspace_ctx).await;
    }

    let dependency_type = args.dependency_config.dependency_type();
    let feature = args.dependency_config.feature.clone();

    let result = match dependency_type {
        DependencyType::CondaDependency(spec_type) => {
            let specs = args.dependency_config.specs()?;
            let names: Vec<String> = specs
                .keys()
                .map(|n| n.as_normalized().to_string())
                .collect();
            (
                workspace_ctx
                    .remove_conda_deps(specs, spec_type, (&args).try_into()?)
                    .await,
                names,
            )
        }
        DependencyType::PypiDependency => {
            let pypi_deps = args.dependency_config.pypi_deps(&workspace)?;
            let names: Vec<String> = pypi_deps
                .keys()
                .map(|n| n.as_source().to_string())
                .collect();
            let pypi_deps: IndexMap<_, _> = pypi_deps
                .into_iter()
                .map(|(name, req)| (name, (req, None, None)))
                .collect();
            (
                workspace_ctx
                    .remove_pypi_deps(pypi_deps, (&args).try_into()?)
                    .await,
                names,
            )
        }
    };

    match result {
        (Ok(()), _) => {
            args.dependency_config
                .display_success("Removed", Default::default());
            Ok(())
        }
        (Err(RemoveError::NotFound { name: missing }), typed_names) => {
            // Show the spelling the user typed, not the manifest's normalized form.
            let name = typed_names
                .iter()
                .find(|n| n.eq_ignore_ascii_case(&missing))
                .cloned()
                .unwrap_or(missing);
            Err(miette::Report::new(DependencyRemovalError::new(
                name,
                (&workspace).workspace_manifest(),
                Scope::Table {
                    dependency_type,
                    feature,
                },
            )))
        }
        (Err(other), _) => Err(miette::Report::new(other)),
    }
}

/// Returns `true` if the user pinned the removal to a specific location with a
/// flag, in which case the strict (single-table) removal path is used.
fn location_specified(config: &DependencyConfig) -> bool {
    config.pypi
        || config.host
        || config.build
        || !config.feature.is_default()
        || !config.platforms.is_empty()
        || config.git.is_some()
}

/// Remove each requested package from wherever it is defined in the workspace.
///
/// Resolution happens up front: every package must resolve to exactly one
/// location or the whole command fails without touching the manifest. A package
/// that is missing yields a "not found" diagnostic with suggestions; one that
/// lives in several tables yields an ambiguity diagnostic listing them.
async fn execute_auto(
    args: &Args,
    workspace: &Workspace,
    workspace_ctx: &WorkspaceContext<CliInterface>,
) -> miette::Result<()> {
    let manifest = workspace.workspace_manifest();

    let mut removals = Vec::with_capacity(args.dependency_config.specs.len());
    for spec in &args.dependency_config.specs {
        let name = bare_name(spec);
        let mut locations = locate(manifest, name);
        match locations.len() {
            0 => {
                return Err(miette::Report::new(DependencyRemovalError::new(
                    spec.clone(),
                    manifest,
                    Scope::Anywhere,
                )));
            }
            1 => removals.push(to_removal(name, locations.pop().expect("one location"))),
            _ => {
                return Err(miette::Report::new(AmbiguousRemovalError::new(
                    spec.clone(),
                    &locations,
                )));
            }
        }
    }

    workspace_ctx
        .remove_resolved(removals, args.try_into()?)
        .await?;
    args.dependency_config
        .display_success("Removed", Default::default());
    Ok(())
}

/// Convert a located dependency into the API's [`Removal`] instruction, keeping
/// the platform-agnostic table and any concrete platform targets separate.
fn to_removal(name: &str, location: Location) -> Removal {
    let default_target = location.platforms.iter().any(Option::is_none);
    let platforms: Vec<Platform> = location.platforms.iter().filter_map(|p| *p).collect();
    match location.slot {
        Slot::Conda(spec_type) => Removal::Conda {
            name: PackageName::try_from(name).expect("located as a conda dependency"),
            spec_type,
            feature: location.feature,
            platforms,
            default_target,
        },
        Slot::Pypi => Removal::Pypi {
            name: PypiPackageName::from_str(name).expect("located as a pypi dependency"),
            feature: location.feature,
            platforms,
            default_target,
        },
    }
}

/// Extract the bare package name from a user-supplied spec, dropping any channel
/// prefix (`conda-forge::`) and version/extra qualifiers so it can be matched
/// against the names stored in the manifest.
fn bare_name(spec: &str) -> &str {
    let without_channel = spec.rsplit("::").next().unwrap_or(spec);
    let end = without_channel
        .find(|c: char| "<>=!~ \t@;[(".contains(c))
        .unwrap_or(without_channel.len());
    without_channel[..end].trim()
}

#[cfg(test)]
mod tests {
    use super::bare_name;

    #[test]
    fn bare_name_strips_qualifiers() {
        assert_eq!(bare_name("numpy"), "numpy");
        assert_eq!(bare_name("numpy>=1.0"), "numpy");
        assert_eq!(bare_name("numpy >=1.0"), "numpy");
        assert_eq!(bare_name("numpy==1.26.*"), "numpy");
        assert_eq!(bare_name("conda-forge::numpy"), "numpy");
        assert_eq!(bare_name("conda-forge::numpy>=1.0"), "numpy");
        assert_eq!(bare_name("black[d]"), "black");
        assert_eq!(bare_name("ruamel.yaml"), "ruamel.yaml");
        assert_eq!(bare_name("foo @ git+https://example.com/foo"), "foo");
    }
}
