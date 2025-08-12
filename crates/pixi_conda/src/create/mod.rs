pub mod input;

use clap::Parser;
use indicatif::ProgressBar;
use input::EnvironmentInput;
use itertools::Itertools;
use miette::{Context, IntoDiagnostic};
use pixi_config::Config;
use pixi_progress::global_multi_progress;
use rattler_conda_types::package::ArchiveIdentifier;
use rattler_conda_types::{
    ChannelConfig, MatchSpec, NamedChannelOrUrl, PackageArchiveHash, PackageName,
    ParseChannelError, Platform,
};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::{collections::HashMap, io, io::Write, path::PathBuf, str::FromStr, time::Instant};
use tabwriter::TabWriter;

use crate::{EnvironmentName, command_dispatcher_builder, registry::Registry};
use pixi_command_dispatcher::{
    BuildEnvironment, CommandDispatcher, InstallPixiEnvironmentSpec, PixiEnvironmentSpec,
};
use pixi_record::PixiRecord;
use pixi_spec;
use pixi_spec::{PixiSpec, UrlSpec};
use pixi_spec_containers::DependencyMap;
use rattler_conda_types::prefix::Prefix;

/// Central coordinator for conda environment creation operations.
///
/// This struct encapsulates all the resolved configuration needed to create a conda
/// environment, from initial specification through final installation. It serves as
/// the main context object that carries validated settings between the different
/// phases of environment creation (resolving, solving, displaying, installing).
///
/// Once created via `resolve()`, an EnvironmentCreator instance represents a fully
/// validated environment specification that can be safely executed without further
/// configuration conflicts or validation errors.
struct EnvironmentCreator {
    prefix: PathBuf,
    environment_name: String,
    input: EnvironmentInput,
    channel_config: ChannelConfig,
    channels: Vec<rattler_conda_types::ChannelUrl>,
    build_environment: BuildEnvironment,
}

/// Command line arguments for creating a new conda environment.
///
/// This struct defines all the options users can specify when creating conda
/// environments through the CLI. It handles the complex interaction between
/// different ways of specifying packages (direct specs vs files), environment
/// targets (names vs explicit paths), and various customization options for
/// channels, platforms, and installation behavior.
///
/// The structure uses clap's derive macros to automatically generate argument
/// parsing, validation, and help text generation from the field annotations.
#[derive(Parser, Debug)]
pub struct Args {
    /// List of packages to install or update in the conda environment.
    #[clap( conflicts_with = "file", num_args = 1.., required = true)]
    package_spec: Vec<MatchSpec>,

    /// Read package versions from the given file. Repeated file specifications
    /// can be passed (e.g. --file=file1 --file=file2).
    #[clap(long, short, conflicts_with = "package_spec", num_args = 1.., required = true)]
    file: Vec<PathBuf>,

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

    /// Sets any confirmation values to 'yes' automatically. Users will not be
    /// asked to confirm any adding, deleting, backups, etc.
    #[clap(long, short, help_heading = "Output, Prompt, and Flow Control Options")]
    yes: bool,

    /// Only display what would have been done.
    #[clap(long, short, help_heading = "Output, Prompt, and Flow Control Options")]
    dry_run: bool,

    #[clap(flatten)]
    channel_customization: ChannelCustomization,
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
struct ChannelCustomization {
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

    /// Use packages built for this platform. The new environment will be
    /// configured to remember this choice. Should be formatted like
    /// 'osx-64', 'linux-32', 'win-64', and so on. Defaults to the
    /// current (native) platform.
    #[clap(long, visible_alias = "subdir", help_heading = "Channel customization")]
    platform: Option<Platform>,
}

/// Executes the complete environment creation workflow.
///
/// This is the main entry point for creating conda environments. It orchestrates
/// the entire process from initial setup through final installation:
/// 1. Sets up the CommandDispatcher for solving and installation operations
/// 2. Resolves all configuration from command line arguments and global config
/// 3. Solves the environment to determine package versions and dependencies
/// 4. Displays the planned transaction to the user
/// 5. Handles dry-run mode if specified
/// 6. Confirms installation with the user (unless --yes is specified)
/// 7. Installs the solved environment to the target prefix
pub async fn execute(config: Config, args: Args) -> miette::Result<()> {
    let command_dispatcher = setup_command_dispatcher(&config)?;
    let env_config = EnvironmentCreator::resolve(&config, &args).await?;
    let solved_records = env_config.solve_environment(&command_dispatcher).await?;

    env_config.display_transaction(&solved_records)?;

    if args.dry_run {
        return Ok(());
    }

    let confirmed = confirm_installation(&args)?;
    if !confirmed {
        eprintln!("Aborting");
        return Ok(());
    }

    env_config
        .install_environment(&command_dispatcher, solved_records)
        .await
}

/// Sets up a CommandDispatcher configured for conda environment operations.
///
/// This function creates a CommandDispatcher with all necessary components for
/// solving and installing conda environments:
/// - Configures progress reporting using the global multi-progress system
/// - Sets up HTTP clients, caches, and repository gateways
/// - Configures platform-specific settings and virtual packages
/// - Enables link scripts execution for proper conda environment setup
///
/// The dispatcher is pre-configured with pixi's TopLevelProgress reporter
/// to provide consistent progress feedback during long-running operations.
fn setup_command_dispatcher(config: &Config) -> miette::Result<CommandDispatcher> {
    let multi_progress = global_multi_progress();
    let anchor_pb = multi_progress.add(ProgressBar::hidden());

    Ok(command_dispatcher_builder(config)
        .map_err(|e| miette::miette!("Failed to create command dispatcher: {}", e))?
        .with_reporter(pixi_reporters::TopLevelProgress::new(
            global_multi_progress(),
            anchor_pb,
        ))
        .finish())
}

impl EnvironmentCreator {
    /// Resolves and validates all configuration needed for environment creation.
    ///
    /// This method transforms raw command line arguments and global configuration into
    /// a fully prepared EnvironmentCreator. It reconciles potentially conflicting inputs
    /// from different sources (CLI args, input files, global config), validates the
    /// target environment setup, and prepares all necessary components for the solving
    /// and installation phases. The result is a self-contained object that knows
    /// exactly what environment to create and how to create it.
    async fn resolve(config: &Config, args: &Args) -> miette::Result<Self> {
        // Parse the input but keep it in its original form
        let input =
            EnvironmentInput::from_files_or_specs(args.file.clone(), args.package_spec.clone())?;

        // Extract input path if it's an environment yaml
        let input_path = match &input {
            EnvironmentInput::EnvironmentYaml(_, path) => Some(path.clone()),
            _ => None,
        };

        // Determine the path to the environment
        let prefix = Self::resolve_prefix(args, &input)?;

        // Handle existing prefix removal
        Self::handle_existing_prefix(&prefix, args.dry_run, args.yes).await?;

        // Set up channel configuration
        let mut channel_config = config.global_channel_config().clone();
        if let Some(input_path) = &input_path {
            channel_config.root_dir = input_path
                .parent()
                .expect("a file must have a parent")
                .to_path_buf();
        }

        // Resolve channels
        let channels = Self::resolve_channels(config, args, &input, &channel_config)?;

        // Determine platform
        let platform = args
            .channel_customization
            .platform
            .unwrap_or_else(Platform::current);

        // Create build environment
        let build_environment = if platform == Platform::current() {
            BuildEnvironment::default()
        } else {
            BuildEnvironment::simple_cross(platform)
                .into_diagnostic()
                .context("failed to create cross-platform build environment")?
        };

        // Determine environment name
        let environment_name = match &input {
            EnvironmentInput::EnvironmentYaml(env_yaml, _) => env_yaml.name.clone(),
            _ => None,
        }
        .or_else(|| {
            prefix
                .file_name()
                .map(OsStr::to_string_lossy)
                .map(Cow::into_owned)
        })
        .unwrap_or_else(|| String::from("env"));

        Ok(EnvironmentCreator {
            prefix,
            environment_name,
            input,
            channel_config,
            channels,
            build_environment,
        })
    }

    /// Determines the target prefix path for the environment.
    ///
    /// This method implements the prefix resolution logic, handling the various ways
    /// users can specify where to create the environment. It prioritizes explicit
    /// paths over names, warns about conflicting specifications, and ensures the
    /// final path is absolute and normalized for consistent behavior across platforms.
    fn resolve_prefix(args: &Args, input: &EnvironmentInput) -> miette::Result<PathBuf> {
        // Extract prefix and name from input if it's an EnvironmentYaml
        let (input_prefix, input_name) = match input {
            EnvironmentInput::EnvironmentYaml(env_yaml, _) => {
                (env_yaml.prefix.as_ref(), env_yaml.name.as_ref())
            }
            _ => (None, None),
        };

        let prefix = if let Some(prefix) = &args.prefix {
            if input_prefix.is_some() {
                tracing::warn!(
                    "--prefix is specified, but the input file also contains a prefix, the input file will be ignored"
                );
            } else if input_name.is_some() {
                tracing::warn!(
                    "--prefix is specified, but the input file also contains a name, the input file will be ignored"
                );
            }
            prefix
        } else if let Some(ref name) = args.name {
            if input_prefix.is_some() {
                tracing::warn!(
                    "--name is specified, but the input file also contains a prefix, the input file will be ignored"
                );
            } else if input_name.is_some() {
                tracing::warn!(
                    "--name is specified, but the input file also contains a name, the input file will be ignored"
                );
            }
            let registry = Registry::from_env();
            &registry.root().join(name.as_ref())
        } else if let Some(prefix) = input_prefix {
            if input_name.is_some() {
                tracing::warn!(
                    "the input file contains both a 'name' and a 'prefix', the 'name' will be ignored"
                );
            }
            prefix
        } else if let Some(name) = input_name {
            let registry = Registry::from_env();
            &registry.root().join(name)
        } else {
            miette::bail!("either --name or --prefix must be specified");
        };

        let prefix = if prefix.is_relative() {
            std::env::current_dir()
                .into_diagnostic()
                .context("failed to determine the current directory")?
                .join(prefix)
        } else {
            prefix.to_path_buf()
        };

        Ok(dunce::simplified(&prefix).to_path_buf())
    }

    /// Handles the case where the target prefix directory already exists.
    ///
    /// In dry-run mode, fails immediately if prefix exists. Otherwise, prompts
    /// the user for confirmation to remove the existing directory (unless --yes
    /// is specified). Removes the directory if confirmed, or fails if declined.
    async fn handle_existing_prefix(
        prefix: &PathBuf,
        dry_run: bool,
        yes: bool,
    ) -> miette::Result<()> {
        if prefix.is_dir() && dry_run {
            miette::bail!(
                "The prefix already exists, and --dry-run is specified, so the operation will not be performed"
            );
        } else if prefix.is_dir() {
            let allow_remove = yes
                || dialoguer::Confirm::new()
                    .with_prompt(format!(
                        "{} The prefix '{}' already exists, do you want to remove it?",
                        console::style("?").blue(),
                        prefix.display()
                    ))
                    .report(false)
                    .default(false)
                    .show_default(true)
                    .interact()
                    .into_diagnostic()?;
            if allow_remove {
                fs_err::remove_dir_all(prefix).into_diagnostic()?;
            } else {
                eprintln!("Aborting");
                miette::bail!("User cancelled due to existing prefix");
            }
        }
        Ok(())
    }

    /// Resolves the final list of conda channels to use for package lookup.
    ///
    /// This method consolidates channel specifications from command line arguments,
    /// input files, and global configuration into a final ordered list. It handles
    /// the override semantics where CLI channels can completely replace input file
    /// channels, and ensures all channel names are converted to proper URLs for
    /// use by the package resolver.
    fn resolve_channels(
        config: &Config,
        args: &Args,
        input: &EnvironmentInput,
        channel_config: &ChannelConfig,
    ) -> miette::Result<Vec<rattler_conda_types::ChannelUrl>> {
        let mut channels = if args.channel_customization.channel.is_empty() {
            config.default_channels()
        } else {
            args.channel_customization.channel.clone()
        };

        // Extract channels from input if it's an EnvironmentYaml
        let mut input_channels = match input {
            EnvironmentInput::EnvironmentYaml(env_yaml, _) => env_yaml.channels.clone(),
            _ => Vec::new(),
        };

        if args.channel_customization.override_channels {
            if !input_channels.is_empty() {
                tracing::warn!(
                    "--override-channels is specified, but the input also contains channels, these will be ignored"
                );
            }
        } else {
            channels.append(&mut input_channels);
        }

        channels
            .into_iter()
            .map(|channel| channel.into_base_url(channel_config))
            .collect::<Result<Vec<_>, ParseChannelError>>()
            .into_diagnostic()
    }

    /// Solves the environment to determine exact package versions and dependencies.
    ///
    /// This method transforms the high-level package specifications into concrete,
    /// installable package records. It creates a solver specification that includes
    /// all the resolved configuration (channels, dependencies, build environment)
    /// and delegates to the CommandDispatcher to perform the actual dependency
    /// resolution. The solver handles version constraints, dependency conflicts,
    /// and platform compatibility to produce a consistent set of packages.
    async fn solve_environment(
        &self,
        command_dispatcher: &CommandDispatcher,
    ) -> miette::Result<Vec<PixiRecord>> {
        // Create dependencies from input
        let dependencies = self.create_dependency_map()?;

        // Create the pixi environment spec
        let environment_spec = PixiEnvironmentSpec {
            name: Some(self.environment_name.clone()),
            dependencies,
            build_environment: self.build_environment.clone(),
            channels: self.channels.clone(),
            channel_config: self.channel_config.clone(),
            ..Default::default()
        };

        // Solve the environment
        let solved_records = command_dispatcher
            .solve_pixi_environment(environment_spec)
            .await?;

        command_dispatcher.clear_reporter().await;
        Ok(solved_records)
    }

    /// Displays the transaction summary showing what packages will be installed.
    ///
    /// This method presents the solved package list to the user in a human-readable
    /// format, showing package names, versions, build strings, channels, and sizes.
    /// It provides transparency about what the installation will do before asking
    /// for confirmation, helping users understand the impact of their request.
    fn display_transaction(&self, records: &[PixiRecord]) -> miette::Result<()> {
        eprintln!("\nThe following packages will be installed:\n");
        print_transaction(
            records,
            &std::collections::HashMap::new(),
            &self.channel_config,
        )
        .into_diagnostic()
    }

    /// Performs the actual installation of the solved environment.
    ///
    /// This method takes the concrete package records from the solver and creates
    /// the conda environment at the target prefix. It configures the installation
    /// specification with all necessary details (prefix, build environment, channels)
    /// and delegates to the CommandDispatcher to handle the complex process of
    /// downloading, extracting, and linking packages into a working environment.
    async fn install_environment(
        &self,
        command_dispatcher: &CommandDispatcher,
        solved_records: Vec<PixiRecord>,
    ) -> miette::Result<()> {
        let package_count = solved_records.len();
        let install_spec = InstallPixiEnvironmentSpec {
            name: self.environment_name.clone(),
            records: solved_records,
            prefix: Prefix::create(self.prefix.clone()).into_diagnostic()?,
            installed: None,
            build_environment: self.build_environment.clone(),
            force_reinstall: std::collections::HashSet::new(),
            channels: self.channels.clone(),
            channel_config: self.channel_config.clone(),
            variants: None,
            enabled_protocols: Default::default(),
        };

        let installation_duration = Instant::now();
        command_dispatcher
            .install_pixi_environment(install_spec)
            .await
            .map_err(|e| miette::miette!("Failed to install pixi environment: {}", e))?;
        let installation_duration = installation_duration.elapsed();

        command_dispatcher.clear_reporter().await;

        eprintln!(
            "{}Installed {package_count} packages into {} {}",
            console::style(console::Emoji("✔ ", "")).green(),
            self.prefix.display(),
            console::style(format!(
                "in {}",
                humantime::format_duration(installation_duration)
            ))
            .dim()
        );

        Ok(())
    }

    /// Creates a dependency map from the environment input.
    ///
    /// This method handles all three types of environment input (EnvironmentYaml,
    /// direct MatchSpecs, and explicit environment files) and converts them directly
    /// to PixiSpec dependencies that can be used by the solver.
    fn create_dependency_map(&self) -> miette::Result<DependencyMap<PackageName, PixiSpec>> {
        match &self.input {
            EnvironmentInput::EnvironmentYaml(env_yaml, _) => {
                self.process_match_specs(env_yaml.match_specs().cloned())
            }
            EnvironmentInput::Specs(specs) => self.process_match_specs(specs.iter().cloned()),
            EnvironmentInput::Files(explicit_specs) => self.process_explicit_specs(explicit_specs),
        }
    }

    /// Processes an iterator of MatchSpecs and converts them to PixiSpec dependencies.
    fn process_match_specs(
        &self,
        specs: impl Iterator<Item = MatchSpec>,
    ) -> miette::Result<DependencyMap<PackageName, PixiSpec>> {
        let mut dependencies = DependencyMap::default();

        for spec in specs {
            if let (Some(name), spec) = spec.into_nameless() {
                let pixi_spec = PixiSpec::from_nameless_matchspec(spec, &self.channel_config);
                dependencies.insert(name, pixi_spec);
            } else {
                return Err(miette::miette!(
                    "Pixi cannot deal with nameless match specs yet. Please specify the package name in the match spec."
                ));
            }
        }

        Ok(dependencies)
    }

    /// Processes explicit environment specs and converts them to PixiSpec dependencies.
    fn process_explicit_specs(
        &self,
        explicit_specs: &[rattler_conda_types::ExplicitEnvironmentSpec],
    ) -> miette::Result<DependencyMap<PackageName, PixiSpec>> {
        let mut dependencies = DependencyMap::default();

        for spec in explicit_specs {
            for entry in &spec.packages {
                let (package_name, pixi_spec) = self.process_explicit_entry(entry)?;
                dependencies.insert(package_name, pixi_spec);
            }
        }

        Ok(dependencies)
    }

    /// Processes a single explicit environment entry and converts it to a PixiSpec.
    fn process_explicit_entry(
        &self,
        entry: &rattler_conda_types::ExplicitEnvironmentEntry,
    ) -> miette::Result<(PackageName, PixiSpec)> {
        // Extract hash from URL fragment
        let hash_result = entry
            .package_archive_hash()
            .into_diagnostic()
            .with_context(|| {
                format!(
                    "failed to extract hash from explicit environment entry URL '{}'",
                    entry.url
                )
            })?;

        // Create UrlSpec with extracted hash
        let mut url_spec = UrlSpec {
            url: entry.url.clone(),
            md5: None,
            sha256: None,
        };

        match hash_result {
            Some(PackageArchiveHash::Md5(md5)) => url_spec.md5 = Some(md5),
            Some(PackageArchiveHash::Sha256(sha256)) => url_spec.sha256 = Some(sha256),
            None => {} // No hash available
        }

        // Extract basic information from the filename of the url
        let archive_identifier = ArchiveIdentifier::try_from_url(&entry.url).ok_or_else(|| {
            miette::miette!(
                "failed to extract archive identifier from URL '{}'",
                entry.url
            )
        })?;

        let package_name = PackageName::from_str(&archive_identifier.name)
            .map_err(|_| miette::miette!("Invalid package name in URL '{}'", entry.url))?;

        Ok((package_name, url_spec.into()))
    }
}

/// Prompts the user to confirm proceeding with the installation.
///
/// This function implements the confirmation step that gives users a chance to
/// review the transaction and decide whether to proceed. It respects the --yes
/// flag to automatically confirm in non-interactive scenarios, but otherwise
/// presents a clear prompt with sensible defaults for user interaction.
fn confirm_installation(args: &Args) -> miette::Result<bool> {
    if args.yes {
        return Ok(true);
    }

    eprintln!(); // Add a newline after the transaction

    dialoguer::Confirm::new()
        .with_prompt(format!(
            "{} Do you want to proceed with the installation?",
            console::style("?").blue()
        ))
        .default(true)
        .show_default(true)
        .report(false)
        .interact()
        .into_diagnostic()
}

/// Formats and prints the package transaction in a tabular format.
///
/// This utility function handles the complex task of presenting package information
/// in a clear, readable table format. It processes both binary and source packages,
/// formats channel information appropriately, handles feature annotations, and
/// uses proper alignment and styling to create professional-looking output that
/// matches conda's standard transaction display format.
fn print_transaction(
    records: &[PixiRecord],
    features: &HashMap<PackageName, Vec<String>>,
    channel_config: &ChannelConfig,
) -> io::Result<()> {
    let heading_style = console::Style::new().bold().white().bright();
    let separator_style = console::Style::new().dim();

    let output = std::io::stderr();
    let mut writer = TabWriter::new(output);
    writeln!(
        writer,
        "  {package}\t{version}\t{build}\t{channel}\t{size}",
        package = heading_style.apply_to("Package"),
        version = heading_style.apply_to("Version"),
        build = heading_style.apply_to("Build"),
        channel = heading_style.apply_to("Channel"),
        size = heading_style.apply_to("Size")
    )?;
    writeln!(
        writer,
        "{}",
        separator_style.apply_to("  -------\t-------\t-----\t-------\t----")
    )?;

    let format_record = |writer: &mut TabWriter<_>, r: &PixiRecord| -> io::Result<()> {
        let location = match r {
            PixiRecord::Binary(r) => r
                .channel
                .as_deref()
                .and_then(|c| NamedChannelOrUrl::from_str(c).ok())
                .and_then(|c| c.into_base_url(channel_config).ok())
                .map(|c| channel_config.canonical_name(c.as_ref()))
                .map(|name| {
                    name.split_once("://")
                        .map(|(_, name)| name.to_string())
                        .unwrap_or(name)
                })
                .map(|name| name.trim_end_matches('/').to_string()),
            PixiRecord::Source(source) => Some(source.source.to_string()),
        };

        write!(writer, "{} ", console::style("+").green())?;

        if let Some(features) = features.get(&r.package_record().name) {
            write!(
                writer,
                "{}[{}]\t",
                r.package_record().name.as_normalized(),
                features.join(", "),
            )?
        } else {
            write!(writer, "{}\t", r.package_record().name.as_normalized(),)?;
        }

        writeln!(
            writer,
            "{}\t{}\t{}\t{}",
            &r.package_record().version,
            &r.package_record().build,
            location.as_deref().unwrap_or_default(),
            r.package_record()
                .size
                .map(|bytes| human_bytes::human_bytes(bytes as f64))
                .unwrap_or_default()
        )
    };

    for package in records.iter().sorted_by_key(|r| &r.package_record().name) {
        format_record(&mut writer, package)?;
    }

    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rattler_conda_types::{ExplicitEnvironmentEntry, ExplicitEnvironmentSpec, ParseStrictness};
    use url::Url;

    // Helper function to create a test EnvironmentCreator
    fn create_test_environment_creator(input: EnvironmentInput) -> EnvironmentCreator {
        EnvironmentCreator {
            prefix: PathBuf::from("/tmp/test"),
            environment_name: "test".to_string(),
            input,
            channel_config: ChannelConfig::default_with_root_dir(PathBuf::from("/tmp")),
            channels: vec![],
            build_environment: BuildEnvironment::default(),
        }
    }

    #[test]
    fn test_create_dependency_map_from_explicit_specs() {
        // Create a test explicit environment spec with URL and MD5 hash
        let url = Url::parse("https://conda.anaconda.org/conda-forge/linux-64/numpy-1.21.0-py39h9894fe3_0.conda#5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b").unwrap();
        let entry = ExplicitEnvironmentEntry { url };
        let spec = ExplicitEnvironmentSpec {
            platform: Some(Platform::Linux64),
            packages: vec![entry],
        };

        let input = EnvironmentInput::Files(vec![spec]);
        let creator = create_test_environment_creator(input);

        // Test the conversion
        let result = creator.create_dependency_map();
        assert!(result.is_ok());

        let dependencies = result.unwrap();
        assert_eq!(dependencies.names().count(), 1);

        // Verify the dependency was created correctly
        let package_name = PackageName::from_str("numpy").unwrap();
        assert!(dependencies.contains_key(&package_name));
    }

    #[test]
    fn test_create_dependency_map_from_explicit_specs_with_sha256() {
        // Create a test explicit environment spec with URL and SHA256 hash
        let url = Url::parse("https://conda.anaconda.org/conda-forge/linux-64/scipy-1.7.0-py39h9894fe3_0.conda#sha256:1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef").unwrap();
        let entry = ExplicitEnvironmentEntry { url };
        let spec = ExplicitEnvironmentSpec {
            platform: Some(Platform::Linux64),
            packages: vec![entry],
        };

        let input = EnvironmentInput::Files(vec![spec]);
        let creator = create_test_environment_creator(input);

        // Test the conversion
        let result = creator.create_dependency_map();
        assert!(result.is_ok());

        let dependencies = result.unwrap();
        assert_eq!(dependencies.names().count(), 1);

        // Verify the dependency was created correctly with SHA256
        let package_name = PackageName::from_str("scipy").unwrap();
        assert!(dependencies.contains_key(&package_name));
    }

    #[test]
    fn test_create_dependency_map_from_explicit_specs_no_hash() {
        // Create a test explicit environment spec with URL but no hash
        let url = Url::parse(
            "https://conda.anaconda.org/conda-forge/linux-64/requests-2.25.1-pyhd3eb1b0_0.conda",
        )
        .unwrap();
        let entry = ExplicitEnvironmentEntry { url };
        let spec = ExplicitEnvironmentSpec {
            platform: Some(Platform::Linux64),
            packages: vec![entry],
        };

        let input = EnvironmentInput::Files(vec![spec]);
        let creator = create_test_environment_creator(input);

        // Test the conversion
        let result = creator.create_dependency_map();
        assert!(result.is_ok());

        let dependencies = result.unwrap();
        assert_eq!(dependencies.names().count(), 1);

        // Verify the dependency was created correctly without hashes
        let package_name = PackageName::from_str("requests").unwrap();
        assert!(dependencies.contains_key(&package_name));
    }

    #[test]
    fn test_create_dependency_map_from_match_specs() {
        // Create direct MatchSpec input
        let match_spec = MatchSpec::from_str("numpy>=1.21.0", ParseStrictness::Lenient).unwrap();
        let input = EnvironmentInput::Specs(vec![match_spec]);
        let creator = create_test_environment_creator(input);

        // Test the conversion
        let result = creator.create_dependency_map();
        assert!(result.is_ok());

        let dependencies = result.unwrap();
        assert_eq!(dependencies.names().count(), 1);

        // Verify the dependency was created correctly
        let package_name = PackageName::from_str("numpy").unwrap();
        assert!(dependencies.contains_key(&package_name));
    }

    #[test]
    fn test_create_dependency_map_from_multiple_explicit_specs() {
        // Create multiple explicit environment specs
        let url1 = Url::parse("https://conda.anaconda.org/conda-forge/linux-64/numpy-1.21.0-py39h9894fe3_0.conda#5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b").unwrap();
        let url2 = Url::parse(
            "https://conda.anaconda.org/conda-forge/linux-64/scipy-1.7.0-py39h9894fe3_0.conda",
        )
        .unwrap();

        let spec1 = ExplicitEnvironmentSpec {
            platform: Some(Platform::Linux64),
            packages: vec![ExplicitEnvironmentEntry { url: url1 }],
        };

        let spec2 = ExplicitEnvironmentSpec {
            platform: Some(Platform::Linux64),
            packages: vec![ExplicitEnvironmentEntry { url: url2 }],
        };

        let input = EnvironmentInput::Files(vec![spec1, spec2]);
        let creator = create_test_environment_creator(input);

        // Test the conversion
        let result = creator.create_dependency_map();
        assert!(result.is_ok());

        let dependencies = result.unwrap();
        assert_eq!(dependencies.names().count(), 2);

        // Verify both dependencies were created
        assert!(dependencies.contains_key(&PackageName::from_str("numpy").unwrap()));
        assert!(dependencies.contains_key(&PackageName::from_str("scipy").unwrap()));
    }
}
