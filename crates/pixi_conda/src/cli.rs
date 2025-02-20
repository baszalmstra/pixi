use crate::{create, run, activate};
use clap::Subcommand;
use pixi_config::Config;

/// Pixi-conda is a tool for managing conda environments.
#[derive(Subcommand, Debug)]
pub enum Args {
    Activate(activate::Args),
    Create(create::Args),
    Run(run::Args),
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let config = Config::load_global();

    match args {
        Args::Activate(args) => activate::execute(config, args).await,
        Args::Create(args) => create::execute(config, args).await,
        Args::Run(args) => run::execute(config, args).await,
    }
}
