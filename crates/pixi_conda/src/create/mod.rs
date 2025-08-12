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
use pixi_command_dispatcher::{BuildEnvironment, InstallPixiEnvironmentSpec, PixiEnvironmentSpec};
use pixi_record::PixiRecord;
use pixi_spec;
use pixi_spec::PixiSpec;
use pixi_spec_containers::DependencyMap;
use rattler_conda_types::prefix::Prefix;

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
    // Create the CommandDispatcher to handle solving and installation with progress reporting
    let multi_progress = global_multi_progress();
    let anchor_pb = multi_progress.add(ProgressBar::hidden());
    let command_dispatcher = command_dispatcher_builder(&config)
        .map_err(|e| miette::miette!("Failed to create command dispatcher: {}", e))?
        .with_reporter(pixi_reporters::TopLevelProgress::new(
            global_multi_progress(),
            anchor_pb,
        ))
        .finish();
    // Convert the input into a canonical form.
    let (mut input, input_path) =
        match EnvironmentInput::from_files_or_specs(args.file, args.package_spec)? {
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

    // Determine the path to the environment.
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
    } else if let Some(name) = args.name {
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
        &std::env::current_dir()
            .into_diagnostic()
            .context("failed to determine the current directory")?
            .join(prefix)
    } else {
        prefix
    };
    let prefix = dunce::simplified(prefix);

    // Remove the prefix if it already exists
    if prefix.is_dir() && args.dry_run {
        miette::bail!(
            "The prefix already exists, and --dry-run is specified, so the operation will not be performed"
        );
    } else if prefix.is_dir() {
        let allow_remove = args.yes
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
            return Ok(());
        }
    }

    // Construct a channel configuration to resolve channel names.
    let mut channel_config = config.global_channel_config().clone();
    if let Some(input_path) = &input_path {
        channel_config.root_dir = input_path
            .parent()
            .expect("a file must have a parent")
            .to_path_buf();
    }

    // Determine the channels to use for package resolution.
    let mut channels = if args.channel_customization.channel.is_empty() {
        config.default_channels()
    } else {
        args.channel_customization.channel
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

    let channels = channels
        .into_iter()
        .map(|channel| channel.into_base_url(&channel_config))
        .collect::<Result<Vec<_>, ParseChannelError>>()
        .into_diagnostic()?;

    // Determine the platform to use for package resolution.
    let platform = args
        .channel_customization
        .platform
        .unwrap_or_else(Platform::current);

    // Create pixi dependencies from match specs
    let mut dependencies = DependencyMap::default();
    for spec in input.match_specs().cloned() {
        if let (Some(name), spec) = spec.into_nameless() {
            let spec = PixiSpec::from_nameless_matchspec(spec, &channel_config);
            dependencies.insert(name, spec);
        } else {
            return Err(miette::miette!(
                "Pixi cannot deal with nameless match specs yet. Please specify the package name in the match spec."
            ));
        }
    }

    // Create the build environment for the target platform
    let build_environment = if platform == Platform::current() {
        BuildEnvironment::default()
    } else {
        BuildEnvironment::simple_cross(platform)
            .into_diagnostic()
            .context("failed to create cross-platform build environment")?
    };

    // Determine the environment name
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

    // Create the pixi environment solve spec
    let solve_spec = PixiEnvironmentSpec {
        name: Some(environment_name.clone()),
        dependencies,
        build_environment: build_environment.clone(),
        channels: channels.clone(),
        channel_config: channel_config.clone(),
        ..Default::default()
    };

    // Solve the pixi environment using CommandDispatcher
    let solved_records = command_dispatcher
        .solve_pixi_environment(solve_spec)
        .await
        .map_err(|e| miette::miette!("Failed to solve pixi environment: {}", e))?;

    // Clear the reporter.
    command_dispatcher.clear_reporter().await;

    // Print the result
    eprintln!("\nThe following packages will be installed:\n");
    print_transaction(
        &solved_records,
        &std::collections::HashMap::new(),
        &channel_config,
    )
    .into_diagnostic()?;

    if args.dry_run {
        // This is the point where we would normally ask the user for confirmation for
        // installation.
        return Ok(());
    }

    eprintln!(); // Add a newline after the transaction

    let do_install = args.yes
        || dialoguer::Confirm::new()
            .with_prompt(format!(
                "{} Do you want to proceed with the installation?",
                console::style("?").blue()
            ))
            .default(true)
            .show_default(true)
            .report(false)
            .interact()
            .into_diagnostic()?;
    if !do_install {
        eprintln!("Aborting");
        return Ok(());
    }

    // Create installation spec
    let package_count = solved_records.len();
    let install_spec = InstallPixiEnvironmentSpec {
        name: environment_name,
        records: solved_records,
        prefix: Prefix::create(prefix.to_path_buf()).into_diagnostic()?,
        installed: None,
        build_environment,
        force_reinstall: std::collections::HashSet::new(),
        channels,
        channel_config: channel_config.clone(),
        variants: None,
        enabled_protocols: Default::default(),
    };

    // Install the environment using CommandDispatcher
    let installation_duration = Instant::now();
    command_dispatcher
        .install_pixi_environment(install_spec)
        .await
        .map_err(|e| miette::miette!("Failed to install pixi environment: {}", e))?;
    let installation_duration = installation_duration.elapsed();

    // Clear the reporter
    command_dispatcher.clear_reporter().await;

    eprintln!(
        "{}Installed {package_count} packages into {} {}",
        console::style(console::Emoji("✔ ", "")).green(),
        prefix.display(),
        console::style(format!(
            "in {}",
            humantime::format_duration(installation_duration)
        ))
        .dim()
    );

    Ok(())
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
