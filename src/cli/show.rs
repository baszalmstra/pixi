use crate::consts::PROJECT_MANIFEST;
use crate::environment::load_or_update_lock_file;
use crate::Project;
use clap::Parser;
use indexmap::IndexMap;
use itertools::Itertools;
use miette::{IntoDiagnostic, LabeledSpan};
use rattler_conda_types::conda_lock::LockedDependency;
use rattler_conda_types::{PackageName, Platform};
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

/// List all packages in the environment.
#[derive(Parser, Debug)]
pub struct Args {
    /// The path to 'pixi.toml'
    #[arg(long)]
    pub manifest_path: Option<PathBuf>,

    /// Require pixi.lock is up-to-date
    #[clap(long, conflicts_with = "frozen")]
    pub locked: bool,

    /// Don't check if pixi.lock is up-to-date, read the lockfile as is
    #[clap(long, conflicts_with = "locked")]
    pub frozen: bool,

    /// The platform to show packages for
    #[clap(long, short)]
    pub platform: Option<Platform>,
}

/// CLI entry point for `pixi show`
pub async fn execute(args: Args) -> miette::Result<()> {
    // Load the project
    let project = Project::load_or_else_discover(args.manifest_path.as_deref())?;

    // Load or update the lockfile
    let lock_file = load_or_update_lock_file(&project, args.frozen, args.locked).await?;

    // Use the passed in platform or the current platform
    let platform = args.platform.unwrap_or_else(Platform::current);

    // Make sure the project supports the current platform. We read the lock-file here instead of
    // the `project` because the user may have specified "frozen" in which case the lock-file might
    // not be up-to-date with the project file.
    if !lock_file.metadata.platforms.contains(&platform) {
        // The lock-file does not contain the platform, but perhaps the project does?
        if !project.platforms().contains(&platform) {
            let span = project.manifest.project.platforms.span();
            return Err(miette::miette!(
                help =
                    format!("No packages found because the project does not support the platform."),
                labels = vec![LabeledSpan::at(
                    span.unwrap_or_default(),
                    format!("add '{platform}' here"),
                )],
                "the project is not configured for '{platform}'"
            )
            .with_source_code(project.source()));
        } else {
            debug_assert!(args.frozen, "lock-file does not contain platform but the project does while frozen was not specified. This must be a bug");
            return Err(miette::miette!(
                format!("The lock-file is out of date with the {PROJECT_MANIFEST} and does not contain packages for {platform} yet.")));
        }
    }

    // Read all packages from the lock_file
    let packages = lock_file.packages_for_platform(platform);

    let requested_packages = project
        .all_dependencies(platform)
        .ok()
        .into_iter()
        .flat_map(IndexMap::into_keys)
        .collect();

    // Write the packages to the output
    let mut writer = tabwriter::TabWriter::new(std::io::stdout());
    write_packages_text(packages, requested_packages, &mut writer).into_diagnostic()?;
    writer.flush().expect("faild to write to stdout");

    Ok(())
}

/// Writes a formatted table to the output with the packages from the lock-file.
pub fn write_packages_text<'a>(
    packages: impl Iterator<Item = &'a LockedDependency>,
    requested_packages: HashSet<PackageName>,
    output: &mut impl Write,
) -> Result<(), std::io::Error> {
    let packages = packages.sorted_by(|a, b| a.name.cmp(&b.name));

    writeln!(
        output,
        "{}\t{}\t{}\n----\t-------\t-----",
        console::style("Name").bold(),
        console::style("Version").bold(),
        console::style("Build").bold()
    )?;
    for package in packages {
        // Define the style for the name
        let mut package_style = console::Style::new();
        if requested_packages.contains(&package.name) {
            package_style = package_style.bold().blue();
        }

        writeln!(
            output,
            "{name}\t{version}\t{build}",
            name = package_style.apply_to(package.name.as_normalized()),
            version = &package.version,
            build = package.build.as_deref().unwrap_or("?")
        )?;
    }
    Ok(())
}
