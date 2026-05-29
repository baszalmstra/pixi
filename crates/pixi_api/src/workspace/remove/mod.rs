use indexmap::IndexMap;
use miette::Diagnostic;
use pixi_core::{
    InstallFilter, UpdateLockFileOptions, Workspace,
    environment::{LockFileUsage, get_update_lock_file_and_prefix},
    lock_file::{ReinstallPackages, UpdateMode},
    workspace::{PypiDeps, WorkspaceMut},
};
use pixi_manifest::{
    DependencyError, FeatureName, FeaturesExt, LoadManifestsError, RemoveDependencyError, SpecType,
};
use pixi_pypi_spec::PypiPackageName;
use rattler_conda_types::{MatchSpec, PackageName, Platform};
use thiserror::Error;

use crate::workspace::DependencyOptions;

#[derive(Debug, Error, Diagnostic)]
pub enum RemoveError {
    #[error("dependency `{name}` was not found")]
    NotFound { name: String },

    #[error(
        "Cannot remove Python while PyPI dependencies exist. Please remove these PyPI dependencies first: {}",
        .pypi_deps.join(", ")
    )]
    PythonHasPypiDependencies { pypi_deps: Vec<String> },

    #[error(transparent)]
    #[diagnostic(transparent)]
    LoadWorkspace(#[from] LoadManifestsError),

    /// `NoDependency` is hoisted to [`Self::NotFound`] by `From<RemoveDependencyError>`.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Manifest(RemoveDependencyError),

    #[error("failed to save the manifest")]
    Save(#[source] std::io::Error),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockFileUpdate(Box<dyn Diagnostic + Send + Sync + 'static>),
}

impl From<RemoveDependencyError> for RemoveError {
    fn from(value: RemoveDependencyError) -> Self {
        match value {
            RemoveDependencyError::Dependency(DependencyError::NoDependency(name)) => {
                RemoveError::NotFound { name }
            }
            other => RemoveError::Manifest(other),
        }
    }
}

pub async fn remove_conda_deps(
    mut workspace: WorkspaceMut,
    specs: IndexMap<PackageName, MatchSpec>,
    spec_type: SpecType,
    options: DependencyOptions,
) -> Result<(), RemoveError> {
    // Prevent removing Python if PyPI dependencies exist
    for name in specs.keys() {
        if name.as_source() == "python" {
            let pypi_deps = workspace
                .workspace()
                .default_environment()
                .pypi_dependencies(None);
            if !pypi_deps.is_empty() {
                return Err(RemoveError::PythonHasPypiDependencies {
                    pypi_deps: pypi_deps
                        .iter()
                        .map(|(name, _)| name.as_source().to_string())
                        .collect(),
                });
            }
        }
    }

    for name in specs.keys() {
        workspace.manifest().remove_dependency(
            name,
            spec_type,
            &options.platforms,
            &options.feature,
        )?;
    }
    let workspace = workspace.save().await.map_err(RemoveError::Save)?;
    update_lock_file_after_remove(&workspace, &options).await
}

pub async fn remove_pypi_deps(
    mut workspace: WorkspaceMut,
    pypi_deps: PypiDeps,
    options: DependencyOptions,
) -> Result<(), RemoveError> {
    for name in pypi_deps.keys() {
        workspace
            .manifest()
            .remove_pypi_dependency(name, &options.platforms, &options.feature)?;
    }

    let workspace = workspace.save().await.map_err(RemoveError::Save)?;
    update_lock_file_after_remove(&workspace, &options).await
}

/// A dependency together with the full location it was resolved to: which
/// feature, which table, and which platform target(s). The CLI's "search
/// everywhere" path produces these once it has pinned down where a bare
/// `pixi remove <pkg>` should act, so the manifest can be edited without
/// re-deriving the location.
#[derive(Debug, Clone)]
pub enum QualifiedDependency {
    Conda {
        name: PackageName,
        spec_type: SpecType,
        feature: FeatureName,
        /// Concrete platform targets the package was found in.
        platforms: Vec<Platform>,
        /// Whether the package is also present in the platform-agnostic table.
        default_target: bool,
    },
    Pypi {
        name: PypiPackageName,
        feature: FeatureName,
        platforms: Vec<Platform>,
        default_target: bool,
    },
}

/// Remove a set of fully-qualified dependencies from the manifest in a single
/// pass, saving and updating the lock file once. The locations are assumed to
/// have been validated by the caller, so a per-platform miss here is a hard
/// error.
pub async fn remove_resolved(
    mut workspace: WorkspaceMut,
    dependencies: Vec<QualifiedDependency>,
    options: DependencyOptions,
) -> Result<(), RemoveError> {
    // Prevent removing Python if PyPI dependencies exist.
    let removing_python = dependencies.iter().any(
        |dep| matches!(dep, QualifiedDependency::Conda { name, .. } if name.as_source() == "python"),
    );
    if removing_python {
        let pypi_deps = workspace
            .workspace()
            .default_environment()
            .pypi_dependencies(None);
        if !pypi_deps.is_empty() {
            return Err(RemoveError::PythonHasPypiDependencies {
                pypi_deps: pypi_deps
                    .iter()
                    .map(|(name, _)| name.as_source().to_string())
                    .collect(),
            });
        }
    }

    for dependency in &dependencies {
        match dependency {
            QualifiedDependency::Conda {
                name,
                spec_type,
                feature,
                platforms,
                default_target,
            } => {
                if *default_target {
                    workspace
                        .manifest()
                        .remove_dependency(name, *spec_type, &[], feature)?;
                }
                if !platforms.is_empty() {
                    workspace
                        .manifest()
                        .remove_dependency(name, *spec_type, platforms, feature)?;
                }
            }
            QualifiedDependency::Pypi {
                name,
                feature,
                platforms,
                default_target,
            } => {
                if *default_target {
                    workspace
                        .manifest()
                        .remove_pypi_dependency(name, &[], feature)?;
                }
                if !platforms.is_empty() {
                    workspace
                        .manifest()
                        .remove_pypi_dependency(name, platforms, feature)?;
                }
            }
        }
    }

    let workspace = workspace.save().await.map_err(RemoveError::Save)?;
    update_lock_file_after_remove(&workspace, &options).await
}

/// Update the lock file (and prefix) after the manifest has been modified by a
/// removal, unless the user asked to keep the lock file untouched.
async fn update_lock_file_after_remove(
    workspace: &Workspace,
    options: &DependencyOptions,
) -> Result<(), RemoveError> {
    // TODO: update all environments touched by this feature defined.
    // updating prefix after removing from toml
    if options.lock_file_usage == LockFileUsage::Update {
        get_update_lock_file_and_prefix(
            &workspace.default_environment(),
            None,
            UpdateMode::Revalidate,
            UpdateLockFileOptions {
                lock_file_usage: options.lock_file_usage,
                no_install: options.no_install,
                max_concurrent_solves: workspace.config().max_concurrent_solves(),
                ..Default::default()
            },
            ReinstallPackages::default(),
            &InstallFilter::default(),
        )
        .await
        .map_err(|e| RemoveError::LockFileUpdate(e.into()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use pixi_core::{Workspace, environment::LockFileUsage};
    use pixi_manifest::FeatureName;
    use pixi_test_utils::format_diagnostic;
    use rattler_conda_types::{MatchSpec, ParseMatchSpecOptions, RepodataRevision};

    use super::*;

    fn workspace_from(toml: &str) -> Workspace {
        // The workspace re-reads its manifest from disk during `modify()`, so
        // we write the TOML to a temp dir and load from that path. The dir is
        // intentionally leaked for the duration of the test.
        let tmp = tempfile::TempDir::new().unwrap().keep();
        let path = tmp.join("pixi.toml");
        fs_err::write(&path, toml).unwrap();
        Workspace::from_path(&path).expect("failed to load workspace")
    }

    fn options() -> DependencyOptions {
        DependencyOptions {
            feature: FeatureName::DEFAULT,
            platforms: vec![],
            no_install: true,
            lock_file_usage: LockFileUsage::Frozen,
        }
    }

    fn conda_spec(name: &str) -> (PackageName, MatchSpec) {
        let spec = MatchSpec::from_str(
            name,
            ParseMatchSpecOptions::lenient().with_repodata_revision(RepodataRevision::V3),
        )
        .unwrap();
        (spec.name.as_exact().unwrap().clone(), spec)
    }

    /// `pixi remove python` while pypi dependencies still reference it should
    /// fail the python guard before touching the manifest.
    #[tokio::test]
    async fn python_guard_triggers_when_pypi_deps_present() {
        let workspace = workspace_from(
            r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64"]

[dependencies]
python = "*"

[pypi-dependencies]
requests = "*"
"#,
        );
        let (name, spec) = conda_spec("python");
        let mut specs = IndexMap::new();
        specs.insert(name, spec);

        let err = remove_conda_deps(workspace.modify().unwrap(), specs, SpecType::Run, options())
            .await
            .unwrap_err();

        insta::assert_snapshot!(
            format_diagnostic(&err),
            @"  × Cannot remove Python while PyPI dependencies exist. Please remove these PyPI dependencies first: requests"
        );
    }

    /// `pixi remove fizzbuzz` against a workspace that doesn't list fizzbuzz
    /// anywhere should land in the typed `NotFound` arm.
    #[tokio::test]
    async fn missing_dep_triggers_not_found() {
        let workspace = workspace_from(
            r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64"]

[dependencies]
ruff = "*"
"#,
        );
        let (name, spec) = conda_spec("fizzbuzz");
        let mut specs = IndexMap::new();
        specs.insert(name, spec);

        let err = remove_conda_deps(workspace.modify().unwrap(), specs, SpecType::Run, options())
            .await
            .unwrap_err();

        insta::assert_snapshot!(
            format_diagnostic(&err),
            @"  × dependency `fizzbuzz` was not found"
        );
        assert!(matches!(err, RemoveError::NotFound { name } if name == "fizzbuzz"));
    }

    fn conda_name(name: &str) -> PackageName {
        PackageName::try_from(name).unwrap()
    }

    /// `remove_resolved` strips packages from several tables — a conda run dep,
    /// a pypi dep, a platform-specific conda dep, and a named feature — in a
    /// single pass, leaving untouched packages in place.
    #[tokio::test]
    async fn remove_resolved_across_tables() {
        let tmp = tempfile::TempDir::new().unwrap().keep();
        let path = tmp.join("pixi.toml");
        fs_err::write(
            &path,
            r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64"]

[dependencies]
numpy = "*"
ruff = "*"

[pypi-dependencies]
black = "*"

[target.linux-64.dependencies]
only-linux = "*"

[feature.dev.dependencies]
pytest = "*"
"#,
        )
        .unwrap();
        let workspace = Workspace::from_path(&path).expect("failed to load workspace");

        let dependencies = vec![
            QualifiedDependency::Conda {
                name: conda_name("numpy"),
                spec_type: SpecType::Run,
                feature: FeatureName::DEFAULT,
                platforms: vec![],
                default_target: true,
            },
            QualifiedDependency::Pypi {
                name: PypiPackageName::from_str("black").unwrap(),
                feature: FeatureName::DEFAULT,
                platforms: vec![],
                default_target: true,
            },
            QualifiedDependency::Conda {
                name: conda_name("only-linux"),
                spec_type: SpecType::Run,
                feature: FeatureName::DEFAULT,
                platforms: vec![Platform::Linux64],
                default_target: false,
            },
            QualifiedDependency::Conda {
                name: conda_name("pytest"),
                spec_type: SpecType::Run,
                feature: FeatureName::from("dev"),
                platforms: vec![],
                default_target: true,
            },
        ];

        remove_resolved(workspace.modify().unwrap(), dependencies, options())
            .await
            .unwrap();

        let content = fs_err::read_to_string(&path).unwrap();
        assert!(!content.contains("numpy"), "numpy should be removed");
        assert!(!content.contains("black"), "black should be removed");
        assert!(
            !content.contains("only-linux"),
            "only-linux should be removed"
        );
        assert!(!content.contains("pytest"), "pytest should be removed");
        // Untouched dependency survives.
        assert!(content.contains("ruff"), "ruff should be kept");
    }

    /// The python guard also fires through the resolved-removal path.
    #[tokio::test]
    async fn remove_resolved_python_guard() {
        let workspace = workspace_from(
            r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64"]

[dependencies]
python = "*"

[pypi-dependencies]
requests = "*"
"#,
        );

        let dependencies = vec![QualifiedDependency::Conda {
            name: conda_name("python"),
            spec_type: SpecType::Run,
            feature: FeatureName::DEFAULT,
            platforms: vec![],
            default_target: true,
        }];

        let err = remove_resolved(workspace.modify().unwrap(), dependencies, options())
            .await
            .unwrap_err();

        insta::assert_snapshot!(
            format_diagnostic(&err),
            @"  × Cannot remove Python while PyPI dependencies exist. Please remove these PyPI dependencies first: requests"
        );
    }
}
