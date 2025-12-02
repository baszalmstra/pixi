use std::str::FromStr;

use clap::Parser;
use miette::{Context, IntoDiagnostic};
use pixi_consts::consts;
use pixi_core::{
    WorkspaceLocator,
    environment::LockFileUsage,
    lock_file::{LockFileDerivedData, UpdateLockFileOptions},
};
use pixi_diff::{LockFileDiff, LockFileJsonDiff};
use pixi_manifest::ExcludeNewer;

use crate::cli_config::NoInstallConfig;
use crate::cli_config::WorkspaceConfig;

/// The environment variable name for the exclude-newer setting.
const PIXI_EXCLUDE_NEWER_ENV: &str = "PIXI_EXCLUDE_NEWER";

/// Gets the effective exclude-newer value, preferring CLI over environment variable.
fn get_exclude_newer(cli_value: Option<ExcludeNewer>) -> miette::Result<Option<ExcludeNewer>> {
    // CLI takes precedence
    if cli_value.is_some() {
        return Ok(cli_value);
    }

    // Check environment variable
    if let Ok(env_value) = std::env::var(PIXI_EXCLUDE_NEWER_ENV) {
        let exclude_newer = ExcludeNewer::from_str(&env_value).map_err(|e| {
            miette::miette!(
                "Invalid value for {PIXI_EXCLUDE_NEWER_ENV} environment variable: {e}"
            )
        })?;
        return Ok(Some(exclude_newer));
    }

    Ok(None)
}

/// Solve environment and update the lock file without installing the
/// environments.
#[derive(Debug, Parser)]
#[clap(arg_required_else_help = false)]
pub struct Args {
    #[clap(flatten)]
    pub workspace_config: WorkspaceConfig,

    #[clap(flatten)]
    pub no_install_config: NoInstallConfig,

    /// Output the changes in JSON format.
    #[clap(long)]
    pub json: bool,

    /// Check if any changes have been made to the lock file.
    /// If yes, exit with a non-zero code.
    #[clap(long)]
    pub check: bool,

    /// Exclude packages released after the specified date.
    ///
    /// This option is ephemeral and will not be recorded in the lock file.
    /// It is useful for reproducible builds without modifying the pixi.toml.
    /// Accepts RFC 3339 timestamps (e.g., "2006-12-02T02:07:43Z") or UTC dates (e.g., "2006-12-02").
    #[clap(long, value_name = "DATE", help_heading = consts::CLAP_UPDATE_OPTIONS)]
    pub exclude_newer: Option<ExcludeNewer>,
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let workspace = WorkspaceLocator::for_cli()
        .with_search_start(args.workspace_config.workspace_locator_start())
        .locate()?;

    // Get the effective exclude-newer value from CLI or environment variable
    let exclude_newer = get_exclude_newer(args.exclude_newer)?;

    // Update the lock-file, and extract it from the derived data to drop additional resources
    // created for the solve.
    // Use the silent version here since update_lock_file() will display the warning.
    let original_lock_file = workspace.load_lock_file().await?.into_lock_file_or_empty();
    let (LockFileDerivedData { lock_file, .. }, lock_updated) = workspace
        .update_lock_file(UpdateLockFileOptions {
            lock_file_usage: LockFileUsage::Update,
            no_install: args.no_install_config.no_install,
            max_concurrent_solves: workspace.config().max_concurrent_solves(),
            exclude_newer: exclude_newer.map(Into::into),
        })
        .await?;

    // Determine the diff between the old and new lock-file.
    let diff = LockFileDiff::from_lock_files(&original_lock_file, &lock_file);

    // Format as json?
    if args.json {
        let diff = LockFileDiff::from_lock_files(&original_lock_file, &lock_file);
        let json_diff = LockFileJsonDiff::new(Some(workspace.named_environments()), diff);
        let json = serde_json::to_string_pretty(&json_diff).expect("failed to convert to json");
        println!("{json}");
    } else if lock_updated {
        eprintln!(
            "{}Updated lock-file",
            console::style(console::Emoji("✔ ", "")).green()
        );
        diff.print()
            .into_diagnostic()
            .context("failed to print lock-file diff")?;
    } else {
        eprintln!(
            "{}Lock-file was already up-to-date",
            console::style(console::Emoji("✔ ", "")).green()
        );
    }

    // Return with a non-zero exit code if `--check` has been passed and the lock
    // file has been updated
    if args.check && !diff.is_empty() {
        std::process::exit(1);
    }

    Ok(())
}
