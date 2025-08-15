use crate::{EnvironmentName, create, run};
use clap::{Parser, Subcommand};
use pixi_config::Config;
use rattler_conda_types::NamedChannelOrUrl;
use std::path::PathBuf;

/// Pixi-conda is a tool for managing conda environments.
#[derive(Subcommand, Debug)]
pub enum Args {
    Create(create::Args),
    Run(run::Args),
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let config = Config::load_global();

    match args {
        Args::Create(args) => create::execute(config, args).await,
        Args::Run(args) => run::execute(config, args).await,
    }
}

/// Channel-related command line options for environment creation.
///
/// This struct groups together all the command line options that control how
/// conda channels are selected and used during environment creation. It provides
/// users with fine-grained control over package sources, including the ability
/// to add custom channels, override default channels entirely, and target
/// specific platforms for cross-compilation scenarios.
///
/// These options work together to determine the final channel configuration
/// that will be passed to the package solver.
#[derive(Parser, Debug)]
pub struct ChannelCustomization {
    /// Additional channel to search for packages.
    #[clap(long, short, help_heading = "Channel customization")]
    channel: Vec<NamedChannelOrUrl>,

    /// Do not search default channels.
    #[clap(
        long,
        help_heading = "Channel customization",
        default_value = "false",
        requires = "channel"
    )]
    override_channels: bool,
}

pub struct TargetEnvironment {
    /// Name of environment.
    #[clap(
        long,
        short,
        help_heading = "Target Environment Specification",
        conflicts_with = "prefix"
    )]
    name: Option<EnvironmentName>,

    /// Path to environment location (i.e. prefix).
    #[clap(long, short, help_heading = "Target Environment Specification")]
    prefix: Option<PathBuf>,
}
