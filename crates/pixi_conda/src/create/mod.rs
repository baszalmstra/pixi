pub mod input;

use clap::Parser;
use indicatif::ProgressBar;
use input::EnvironmentInput;
use itertools::Itertools;
use miette::{Context, IntoDiagnostic};
use pixi_config::Config;
use pixi_progress::global_multi_progress;
use rattler_conda_types::{
    ChannelConfig, EnvironmentYaml, MatchSpec, MatchSpecOrSubSection, NamedChannelOrUrl,
    PackageName, ParseChannelError, Platform,
};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::{collections::HashMap, io, io::Write, path::PathBuf, str::FromStr, time::Instant};
use tabwriter::TabWriter;

use crate::{EnvironmentName, command_dispatcher_builder, registry::Registry};
use pixi_command_dispatcher::{BuildEnvironment, CommandDispatcher, InstallPixiEnvironmentSpec, PixiEnvironmentSpec};
use pixi_record::PixiRecord;
use pixi_spec;
use pixi_spec::PixiSpec;
use pixi_spec_containers::DependencyMap;
use rattler_conda_types::prefix::Prefix;

/// Handles the complete workflow for creating a conda environment
struct EnvironmentCreator {
    prefix: PathBuf,
    environment_name: String,
    input: EnvironmentYaml,
    channel_config: ChannelConfig,
    channels: Vec<rattler_conda_types::ChannelUrl>,
    build_environment: BuildEnvironment,
}

/// Create a new conda environment from a list of specified packages.
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
    
    env_config.install_environment(&command_dispatcher, solved_records).await
}

/// Set up the CommandDispatcher with progress reporting
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
    /// Resolve all environment configuration from command line args and config
    async fn resolve(config: &Config, args: &Args) -> miette::Result<Self> {
        // Convert the input into a canonical form
        let (mut input, input_path) =
            match EnvironmentInput::from_files_or_specs(args.file.clone(), args.package_spec.clone())? {
                EnvironmentInput::EnvironmentYaml(environment, path) => (environment, Some(path)),
                EnvironmentInput::Specs(specs) => (
                    EnvironmentYaml {
                        dependencies: specs
                            .into_iter()
                            .map(|spec| MatchSpecOrSubSection::MatchSpec(Box::new(spec)))
                            .collect(),
                        ..EnvironmentYaml::default()
                    },
                    None,
                ),
                EnvironmentInput::Files(_) => {
                    unimplemented!("explicit environment files are not yet supported")
                }
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
        let channels = Self::resolve_channels(config, args, &mut input, &channel_config)?;

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
        let environment_name = input
            .name
            .clone()
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

    fn resolve_prefix(args: &Args, input: &EnvironmentYaml) -> miette::Result<PathBuf> {
        let prefix = if let Some(prefix) = &args.prefix {
            if input.prefix.is_some() {
                tracing::warn!(
                    "--prefix is specified, but the input file also contains a prefix, the input file will be ignored"
                );
            } else if input.name.is_some() {
                tracing::warn!(
                    "--prefix is specified, but the input file also contains a name, the input file will be ignored"
                );
            }
            prefix
        } else if let Some(ref name) = args.name {
            if input.prefix.is_some() {
                tracing::warn!(
                    "--name is specified, but the input file also contains a prefix, the input file will be ignored"
                );
            } else if input.name.is_some() {
                tracing::warn!(
                    "--name is specified, but the input file also contains a name, the input file will be ignored"
                );
            }
            let registry = Registry::from_env();
            &registry.root().join(name.as_ref())
        } else if let Some(prefix) = &input.prefix {
            if input.name.is_some() {
                tracing::warn!(
                    "the input file contains both a 'name' and a 'prefix', the 'name' will be ignored"
                );
            }
            prefix
        } else if let Some(name) = &input.name {
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

    async fn handle_existing_prefix(prefix: &PathBuf, dry_run: bool, yes: bool) -> miette::Result<()> {
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

    fn resolve_channels(
        config: &Config,
        args: &Args,
        input: &mut EnvironmentYaml,
        channel_config: &ChannelConfig,
    ) -> miette::Result<Vec<rattler_conda_types::ChannelUrl>> {
        let mut channels = if args.channel_customization.channel.is_empty() {
            config.default_channels()
        } else {
            args.channel_customization.channel.clone()
        };
        
        if args.channel_customization.override_channels {
            if !input.channels.is_empty() {
                tracing::warn!(
                    "--override-channels is specified, but the input also contains channels, these will be ignored"
                );
            }
        } else {
            channels.append(&mut input.channels);
        }

        channels
            .into_iter()
            .map(|channel| channel.into_base_url(channel_config))
            .collect::<Result<Vec<_>, ParseChannelError>>()
            .into_diagnostic()
    }

    /// Solve the environment using CommandDispatcher
    async fn solve_environment(
        &self,
        command_dispatcher: &CommandDispatcher,
    ) -> miette::Result<Vec<PixiRecord>> {
        // Create dependencies from match specs
        let mut dependencies = DependencyMap::default();
        for spec in self.input.match_specs().cloned() {
            if let (Some(name), spec) = spec.into_nameless() {
                let spec = PixiSpec::from_nameless_matchspec(spec, &self.channel_config);
                dependencies.insert(name, spec);
            } else {
                return Err(miette::miette!(
                    "Pixi cannot deal with nameless match specs yet. Please specify the package name in the match spec."
                ));
            }
        }

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

    /// Display the transaction that will be performed
    fn display_transaction(&self, records: &[PixiRecord]) -> miette::Result<()> {
        eprintln!("\nThe following packages will be installed:\n");
        print_transaction(records, &std::collections::HashMap::new(), &self.channel_config)
            .into_diagnostic()
    }

    /// Install the solved environment
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
}

/// Confirm installation with user
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
