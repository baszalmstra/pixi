use std::path::PathBuf;

use clap::{Parser, Subcommand};
use miette::IntoDiagnostic;
use pixi_core::WorkspaceLocator;

use crate::cli_config::WorkspaceConfig;

/// Manage the experimental per-workspace Pixi daemon.
#[derive(Parser, Debug)]
pub struct Args {
    #[clap(flatten)]
    pub workspace_config: WorkspaceConfig,

    /// Workspace root to serve or inspect.
    #[clap(long, hide = true, value_name = "PATH")]
    pub root: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the workspace daemon in the foreground.
    Start,
    /// Show whether the workspace daemon is accepting connections.
    Status,
    /// Ask the workspace daemon to stop.
    Stop,
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let root = daemon_root(&args)?;
    match args.command {
        Command::Start => start(root).await,
        Command::Status => status(root).await,
        Command::Stop => stop(root).await,
    }
}

async fn start(root: PathBuf) -> miette::Result<()> {
    pixi_daemon::serve_workspace(root).await.into_diagnostic()
}

async fn status(root: PathBuf) -> miette::Result<()> {
    let status = pixi_daemon::status_workspace(root)
        .await
        .into_diagnostic()?;
    if let Some(pong) = status.pong {
        println!(
            "running\n  root: {}\n  endpoint: {}\n  protocol: {}",
            pong.workspace_root.display(),
            status.endpoint.display_address(),
            pong.protocol_version,
        );
    } else {
        println!(
            "not running\n  root: {}\n  endpoint: {}",
            status.endpoint.workspace_root().display(),
            status.endpoint.display_address(),
        );
    }
    Ok(())
}

async fn stop(root: PathBuf) -> miette::Result<()> {
    let result = pixi_daemon::stop_workspace(root).await.into_diagnostic()?;
    if result.stopped {
        println!(
            "stopped\n  root: {}\n  endpoint: {}",
            result.endpoint.workspace_root().display(),
            result.endpoint.display_address(),
        );
    } else {
        println!(
            "not running\n  root: {}\n  endpoint: {}",
            result.endpoint.workspace_root().display(),
            result.endpoint.display_address(),
        );
    }
    Ok(())
}

fn daemon_root(args: &Args) -> miette::Result<PathBuf> {
    if let Some(root) = &args.root {
        return Ok(root.clone());
    }

    Ok(WorkspaceLocator::for_cli()
        .with_search_start(args.workspace_config.workspace_locator_start())
        .locate()?
        .root()
        .to_path_buf())
}
