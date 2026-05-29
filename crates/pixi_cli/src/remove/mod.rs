mod error;
mod locate;

use std::str::FromStr;

use clap::Parser;
use indexmap::IndexMap;
use pixi_api::{
    WorkspaceContext,
    workspace::{DependencyOptions, QualifiedDependency, RemoveError},
};
use pixi_config::ConfigCli;
use pixi_core::{DependencyType, Workspace, WorkspaceLocator};
use pixi_manifest::{HasWorkspaceManifest, SpecType, WorkspaceManifest};
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
/// A bare `pixi remove <pkg>...` searches every dependency table (conda and
/// PyPI, every feature, every platform) and removes each package from wherever
/// it is defined. If the package is in the default feature's dependencies it is
/// removed from there; otherwise, if it resolves to a single other location it
/// is removed from that location, and if it is found in several it is reported
/// as ambiguous. Passing a location flag (`--pypi`, `--host`, `--build`,
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

    let mut dependencies = Vec::with_capacity(args.dependency_config.specs.len());
    for spec in &args.dependency_config.specs {
        match resolve(manifest, bare_name(spec)) {
            Resolution::Resolved(dependency) => dependencies.push(dependency),
            Resolution::NotFound => {
                return Err(miette::Report::new(DependencyRemovalError::new(
                    spec.clone(),
                    manifest,
                    Scope::Anywhere,
                )));
            }
            Resolution::Ambiguous(locations) => {
                return Err(miette::Report::new(AmbiguousRemovalError::new(
                    spec.clone(),
                    &locations,
                )));
            }
        }
    }

    workspace_ctx
        .remove_qualified_dependencies(dependencies, args.try_into()?)
        .await?;
    args.dependency_config
        .display_success("Removed", Default::default());
    Ok(())
}

/// The outcome of resolving one requested package against the workspace.
enum Resolution {
    Resolved(QualifiedDependency),
    Ambiguous(Vec<Location>),
    NotFound,
}

/// Resolve a single package name to the location it should be removed from.
///
/// A bare `pixi remove <pkg>` has always targeted the default feature's
/// run-dependencies, so if the package lives there we remove it from there and
/// ignore any other matches (otherwise the ambiguity hint would just repeat the
/// command the user already ran). Only when the default target does not hold the
/// package do we fall back to the single other match, or report ambiguity.
fn resolve(manifest: &WorkspaceManifest, name: &str) -> Resolution {
    let mut locations = locate(manifest, name);
    if let Some(pos) = locations.iter().position(is_default_run_dependency) {
        return Resolution::Resolved(to_qualified_dependency(name, locations.swap_remove(pos)));
    }
    match locations.len() {
        0 => Resolution::NotFound,
        1 => Resolution::Resolved(to_qualified_dependency(
            name,
            locations.pop().expect("one location"),
        )),
        _ => Resolution::Ambiguous(locations),
    }
}

/// Whether a location is the implicit target of a bare `pixi remove <pkg>`: the
/// default feature's `[dependencies]` table.
fn is_default_run_dependency(location: &Location) -> bool {
    location.feature.is_default() && location.slot == Slot::Conda(SpecType::Run)
}

/// Convert a located dependency into the API's [`QualifiedDependency`], keeping
/// the platform-agnostic table and any concrete platform targets separate.
fn to_qualified_dependency(name: &str, location: Location) -> QualifiedDependency {
    let default_target = location.platforms.iter().any(Option::is_none);
    let platforms: Vec<Platform> = location.platforms.iter().filter_map(|p| *p).collect();
    match location.slot {
        Slot::Conda(spec_type) => QualifiedDependency::Conda {
            name: PackageName::try_from(name).expect("located as a conda dependency"),
            spec_type,
            feature: location.feature,
            platforms,
            default_target,
        },
        Slot::Pypi => QualifiedDependency::Pypi {
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
    use std::path::Path;

    use super::*;

    fn parse(toml: &str) -> WorkspaceManifest {
        WorkspaceManifest::from_toml_str_with_base_dir(toml, Path::new(""))
            .expect("failed to parse manifest")
    }

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

    /// When the package is in the default feature's `[dependencies]` we remove
    /// it from there even if it also appears elsewhere, instead of reporting an
    /// ambiguity (which would only echo back the command already run).
    #[test]
    fn resolve_prefers_default_run_dependencies() {
        let manifest = parse(
            r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64"]

[dependencies]
numpy = "*"

[feature.dev.dependencies]
numpy = "*"
"#,
        );
        match resolve(&manifest, "numpy") {
            Resolution::Resolved(QualifiedDependency::Conda {
                spec_type, feature, ..
            }) => {
                assert_eq!(spec_type, SpecType::Run);
                assert!(feature.is_default());
            }
            _ => panic!("expected resolution to the default run-dependencies"),
        }
    }

    /// A single non-default match resolves to that location.
    #[test]
    fn resolve_single_non_default_location() {
        let manifest = parse(
            r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64"]

[feature.dev.dependencies]
numpy = "*"
"#,
        );
        match resolve(&manifest, "numpy") {
            Resolution::Resolved(QualifiedDependency::Conda { feature, .. }) => {
                assert_eq!(feature.as_str(), "dev");
            }
            _ => panic!("expected resolution to feature `dev`"),
        }
    }

    /// Several matches, none in the default `[dependencies]`, is ambiguous.
    #[test]
    fn resolve_ambiguous_without_default() {
        let manifest = parse(
            r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64"]

[pypi-dependencies]
requests = "*"

[feature.dev.dependencies]
requests = "*"
"#,
        );
        match resolve(&manifest, "requests") {
            Resolution::Ambiguous(locations) => assert_eq!(locations.len(), 2),
            _ => panic!("expected an ambiguous resolution"),
        }
    }

    #[test]
    fn resolve_missing_is_not_found() {
        let manifest = parse(
            r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64"]

[dependencies]
numpy = "*"
"#,
        );
        assert!(matches!(resolve(&manifest, "absent"), Resolution::NotFound));
    }
}
