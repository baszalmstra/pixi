use std::path::PathBuf;

use miette::Diagnostic;
use pixi_build_frontend::BackendOverride;
use pixi_command_dispatcher::{CacheDirs, CommandDispatcher, CommandDispatcherBuilder, Limits};
use pixi_config::{Config, get_cache_dir};
use pixi_utils::reqwest::build_reqwest_clients;
use rattler_conda_types::Platform;
use rattler_virtual_packages::{VirtualPackageOverrides, VirtualPackages};
use thiserror::Error;

/// Errors that can occur when building a CommandDispatcher for pixi-conda operations.
///
/// This error type captures all the various failure modes that can occur during
/// CommandDispatcher setup, from filesystem issues to network configuration problems.
/// Each variant provides specific context about what went wrong during the
/// initialization process, helping users and developers diagnose setup issues.
#[derive(Error, Debug, Diagnostic)]
pub enum CommandDispatcherError {
    #[error("Failed to get cache directory: {0}")]
    CacheDirError(String),

    #[error("Failed to build HTTP clients: {0}")]
    HttpClientError(String),

    #[error("Failed to determine current directory")]
    CurrentDirError(#[from] std::io::Error),

    #[error("Failed to detect virtual packages")]
    VirtualPackageError(#[from] rattler_virtual_packages::DetectVirtualPackageError),

    #[error("Failed to load backend overrides: {0}")]
    BackendOverrideError(String),
}

/// Creates a pre-configured CommandDispatcher builder suitable for pixi-conda operations.
///
/// This function sets up a CommandDispatcher with sensible defaults for conda environment
/// management, similar to how the main pixi workspace creates its dispatcher but simplified
/// for standalone conda operations.
pub fn command_dispatcher_builder(
    config: &Config,
) -> Result<CommandDispatcherBuilder, CommandDispatcherError> {
    // Create cache directories for conda operations
    let cache_dirs = CacheDirs::new(
        get_cache_dir().map_err(|e| CommandDispatcherError::CacheDirError(e.to_string()))?,
    );

    // Build HTTP clients for network operations
    let (_client, client_with_middleware) = build_reqwest_clients(Some(config), None)
        .map_err(|e| CommandDispatcherError::HttpClientError(e.to_string()))?;

    // Create the gateway for querying conda repositories
    let gateway = config
        .gateway()
        .with_client(client_with_middleware.clone())
        .with_cache_dir(cache_dirs.root().clone())
        .finish();

    // Determine the tool platform and virtual packages
    let tool_platform = config.tool_platform();
    let tool_virtual_packages =
        if tool_platform.only_platform() == Platform::current().only_platform() {
            // If the tool platform is the same as the current platform, we just assume the
            // same virtual packages apply.
            VirtualPackages::detect(&VirtualPackageOverrides::from_env())?
                .into_generic_virtual_packages()
                .collect()
        } else {
            vec![]
        };

    // Use current directory as root dir for relative path resolution
    let root_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    Ok(CommandDispatcher::builder()
        .with_gateway(gateway)
        .with_cache_dirs(cache_dirs)
        .with_root_dir(root_dir)
        .with_download_client(client_with_middleware)
        .with_limits(Limits::default())
        .with_backend_overrides(
            BackendOverride::from_env()
                .map_err(|e| CommandDispatcherError::BackendOverrideError(e.to_string()))?
                .unwrap_or_default(),
        )
        .execute_link_scripts(true) // Enable link scripts by default for conda environments
        .with_tool_platform(tool_platform, tool_virtual_packages))
}
