use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use itertools::Itertools;
use miette::{Diagnostic, NamedSource, Report};
use pixi_manifest::{
    utils::WithSourceCode, DiscoveredWorkspace, DiscoveredWorkspaceError, ProvenanceError,
    TomlError, WorkspaceDiscoveryError,
};
use thiserror::Error;

use crate::workspace::Workspace;

/// A helper struct that helps discover the workspace root and potentially the
/// "current" package.
pub struct WorkspaceLocator {
    path: Option<PathBuf>,
    with_closest_package: bool,
    emit_warnings: bool,
    consider_environment: bool,
}

#[derive(Debug, Error, Diagnostic)]
pub enum WorkspaceLocatorError {
    /// An IO error occurred while trying to discover the workspace.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Failed to determine the current directory.
    #[error("failed to determine the current directory")]
    CurrentDir(#[source] std::io::Error),

    /// A TOML parsing error occurred while trying to discover the workspace.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Toml(#[from] WithSourceCode<TomlError, NamedSource<Arc<str>>>),

    /// The workspace could not be located.
    #[error(
        "could not find {project_manifest} or {pyproject_manifest} at directory {0}",
        project_manifest = consts::PROJECT_MANIFEST,
        pyproject_manifest = consts::PYPROJECT_MANIFEST
    )]
    WorkspaceNotFound(PathBuf),

    /// The manifest file could not be loaded.
    #[error("could not load '{0}' as a manifest")]
    ProvenanceError(PathBuf, #[source] ProvenanceError),
}

impl WorkspaceLocator {
    /// Constructs a new instance.
    pub fn new() -> Self {
        Self {
            path: None,
            with_closest_package: false,
            emit_warnings: false,
            consider_environment: false,
        }
    }

    /// Constructs a new instance tailored for finding the workspace for CLI
    /// commands.
    pub fn for_cli() -> Self {
        Self::new()
            .with_emit_warnings(true)
            .with_consider_environment(true)
    }

    /// Set the path to start searching for the workspace. If this is not set
    /// the current directory will be used.
    pub fn with_path(self, path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            ..self
        }
    }

    /// Also search for the closest package in the workspace.
    pub fn with_closest_package(self, with_closest_package: bool) -> Self {
        Self {
            with_closest_package,
            ..self
        }
    }

    /// Set whether to emit warnings that are encountered during the discovery
    /// process.
    pub fn with_emit_warnings(self, emit_warnings: bool) -> Self {
        Self {
            emit_warnings,
            ..self
        }
    }

    /// Whether to consider any environment variables that may be set that could
    /// influence the discovery process.
    pub fn with_consider_environment(self, consider_environment: bool) -> Self {
        Self {
            consider_environment,
            ..self
        }
    }

    /// Called to locate the workspace or error out if none could be located.
    pub fn discover(self) -> Result<Workspace, WorkspaceLocatorError> {
        // Determine the search root
        let path = match self.path {
            Some(path) => path,
            None => std::env::current_dir().map_err(WorkspaceLocatorError::CurrentDir)?,
        };

        // Discover the workspace manifest for the current path.
        let mut workspace_manifests =
            match pixi_manifest::WorkspaceDiscoverer::new(path.to_path_buf())
                .with_closest_package(self.with_closest_package)
                .discover()
            {
                Ok(manifests) => manifests,
                Err(WorkspaceDiscoveryError::Toml(err)) => {
                    return Err(WorkspaceLocatorError::Toml(err))
                }
                Err(WorkspaceDiscoveryError::Io(err)) => {
                    return Err(WorkspaceLocatorError::Io(err))
                }
            };

        // Take into consideration any environment variables that may be set.
        if self.consider_environment {
            workspace_manifests =
                Self::apply_environment_overrides(workspace_manifests, self.emit_warnings)?;
        }

        // Early out if discovery failed.
        let Some(discovered_workspace) = workspace_manifests else {
            return Err(WorkspaceLocatorError::WorkspaceNotFound(path));
        };

        // Emit any warnings that were encountered during the discovery process.
        if self.emit_warnings && !discovered_workspace.warnings.is_empty() {
            let warnings = &discovered_workspace.warnings;
            tracing::warn!(
                "Encountered {} warning{} while parsing the manifest:\n{}",
                warnings.len(),
                if warnings.len() == 1 { "" } else { "s" },
                warnings
                    .into_iter()
                    .map(|warning| Report::from(warning))
                    .format_with("\n", |w, f| f(&format_args!("{:?}", w)))
            );
        }

        Ok(discovered_workspace)
    }

    /// Apply any environment overrides to a potentially discovered workspace.
    fn apply_environment_overrides(
        discovered_workspace: Option<DiscoveredWorkspace>,
        emit_warnings: bool,
    ) -> Result<Option<DiscoveredWorkspace>, WorkspaceLocatorError> {
        let env_manifest_path = std::env::var("PIXI_PROJECT_MANIFEST")
            .map(PathBuf::from)
            .ok();

        // Warn the user if they are currently in a shell of another workspace.
        if let Some(workspace_manifests) = &discovered_workspace {
            let discovered_manifest_path = &workspace_manifests.workspace_manifest.provenance.path;
            let in_shell = std::env::var("PIXI_IN_SHELL").is_ok();
            if let Some(env_manifest_path) = env_manifest_path {
                if &env_manifest_path != discovered_manifest_path && in_shell && emit_warnings {
                    tracing::warn!(
                            "Using local manifest {} rather than {} from environment variable `PIXI_PROJECT_MANIFEST`",
                            discovered_manifest_path.display(),
                            env_manifest_path.display(),
                        );
                }
            }
        // Else, if we didn't find a workspace manifest, but we there is an
        // active one set in the environment, we try to use that instead.
        } else if let Some(env_manifest_path) = env_manifest_path {
            match DiscoveredWorkspace::from_workspace_manifest_path(env_manifest_path.clone()) {
                Ok(workspace) => return Ok(Some(workspace)),
                Err(DiscoveredWorkspaceError::Io(err)) => {
                    return Err(WorkspaceLocatorError::Io(err))
                }
                Err(DiscoveredWorkspaceError::Toml(err)) => {
                    return Err(WorkspaceLocatorError::Toml(err))
                }
                Err(DiscoveredWorkspaceError::ProvenanceError(err)) => {
                    return Err(WorkspaceLocatorError::ProvenanceError(
                        env_manifest_path,
                        err,
                    ))
                }
            }
        }

        Ok(discovered_workspace)
    }
}
