use clap::{Arg, Command, CommandFactory};
use fs_err as fs;
use itertools::Itertools;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::str::FromStr;

const MD_EXTENSION: &str = ".md";

/// Generates documentation for the Pixi CLI by:
/// - Loading the clap command from the pixi crate
/// - Creating markdown files for each command
/// - Organizing commands into a directory structure
fn main() -> Result<(), Box<dyn Error>> {
    let command = get_command();

    let output_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../docs/reference/cli");

    println!("Generating CLI documentation in {}", output_dir.display());

    process_subcommands(&command, Vec::new(), &output_dir)?;

    Ok(())
}

/// Converts a command to Markdown format
fn subcommand_to_md(parents: &[String], command: &Command) -> String {
    let mut buffer = String::with_capacity(1024);

    // Start with autogenerated header
    // This is also used in the `ci.yml` to remove all the autogenerated files before running this script.
    writeln!(
        buffer,
        "<!--- This file is autogenerated. Do not edit manually! -->"
    )
    .unwrap();

    // Parent path for relative links
    let parent_path = if parents.is_empty() {
        "".to_string()
    } else {
        format!("{}/", parents.join("/"))
    };

    // Name with correct relative links including .md extension
    let mut name_parts = Vec::new();
    let depth = parents.len() + 1; // Total depth including current command

    // Handle parents
    for (i, parent) in parents.iter().enumerate() {
        let ups = depth - i - 1;
        let relative_path = if ups > 0 {
            format!("{}{}.md", "../".repeat(ups), parent)
        } else {
            format!("{}.md", parent)
        };
        name_parts.push(format!("[{}]({}) ", parent, relative_path));
    }

    // Add current command without link
    name_parts.push(command.get_name().to_string());

    // Title
    writeln!(buffer, "# <code>{}</code>", name_parts.join("")).unwrap();

    // About
    if command.get_name() != "pixi" {
        if let Some(about) = command.get_about() {
            writeln!(buffer, "\n## About").unwrap();
            writeln!(buffer, "{}", about).unwrap();
        }
    }

    // Additional description
    writeln!(
        buffer,
        "\n--8<-- \"docs/reference/cli/{}{}_extender.md:description\"",
        parent_path,
        command.get_name()
    )
    .unwrap();

    // Usage
    writeln!(buffer, "\n## Usage").unwrap();
    writeln!(buffer, "```").unwrap();
    {
        let mut command = command.clone();
        writeln!(
            buffer,
            "{}{}{}",
            parents.join(" "),
            if parents.is_empty() { "" } else { " " },
            command
                .render_usage()
                .to_string()
                .trim_start_matches("Usage: ")
        )
        .unwrap();
    }
    writeln!(buffer, "```").unwrap();

    // Subcommands
    if command.has_subcommands() {
        writeln!(buffer, "\n## Subcommands").unwrap();
        let subcommands: Vec<_> = command.get_subcommands().collect();
        if !subcommands.is_empty() {
            writeln!(
                buffer,
                "{}",
                subcommands_table(subcommands, command.get_name())
            )
            .unwrap();
        }
    }

    // Get a list of the parents and command name
    let mut parts = parents.to_vec();
    parts.push(command.get_name().to_string());

    // Positionals
    let positionals: Vec<_> = command.get_positionals().collect();
    if !positionals.is_empty() {
        writeln!(buffer, "\n## Arguments").unwrap();
        write!(buffer, "{}", arguments(&positionals, &parts)).unwrap();
    }

    // Options
    let opts: Vec<_> = command.get_opts().collect();
    if !opts.is_empty() {
        // All options with their header
        let header_option_map: HashMap<String, Vec<&Arg>> = opts
            .iter()
            .map(|o| {
                (
                    o.get_help_heading().unwrap_or("Options").to_string(),
                    o.to_owned(),
                )
            })
            .into_group_map();

        let sorted_header_options = header_option_map.iter().sorted_by(|a, b| {
            match (a.0.as_str(), b.0.as_str()) {
                // "Options" comes first
                ("Options", _) => Ordering::Less,
                (_, "Options") => Ordering::Greater,
                // "Global Options" comes last
                ("Global Options", _) => Ordering::Greater,
                (_, "Global Options") => Ordering::Less,
                // Alphabetical for others
                _ => a.0.cmp(b.0),
            }
        });

        for (header, opts) in sorted_header_options {
            writeln!(buffer, "\n## {}", header).unwrap();
            write!(buffer, "{}", arguments(opts, &parts)).unwrap();
        }
    }

    // Long about
    if let Some(long) = command.get_long_about() {
        writeln!(buffer, "\n## Description").unwrap();
        writeln!(buffer, "{}\n", long.to_string().trim_end()).unwrap();
    }

    // Write snippet link
    writeln!(
        buffer,
        "\n--8<-- \"docs/reference/cli/{}{}_extender.md:example\"",
        parent_path,
        command.get_name()
    )
    .unwrap();

    buffer
}

/// Processes a command and its subcommands, generating markdown documentation
fn process_subcommands(
    command: &Command,
    parent_path: Vec<String>,
    output_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut current_path = parent_path;
    current_path.push(command.get_name().to_string());

    let command_file_name = format!("{}{}", current_path.join("/"), MD_EXTENSION);
    let command_file_path = output_dir.join(&command_file_name);

    fs::create_dir_all(command_file_path.parent().ok_or("Invalid path")?)
        .map_err(|e| format!("Failed to create directories: {}", e))?;
    fs::write(
        &command_file_path,
        subcommand_to_md(&current_path[..current_path.len() - 1], command),
    )
    .map_err(|e| {
        format!(
            "Failed to write command file {}: {}",
            command_file_path.display(),
            e
        )
    })?;

    for subcommand in command.get_subcommands() {
        process_subcommands(subcommand, current_path.clone(), output_dir)?;
    }

    Ok(())
}

/// Generates a Markdown table of subcommands with command names as links to their pages
fn subcommands_table(subcommands: Vec<&Command>, parent: &str) -> String {
    let mut buffer = String::with_capacity(1024);
    writeln!(buffer, "| Command | Description |").unwrap();
    writeln!(buffer, "|---------|-------------|").unwrap();
    for subcommand in subcommands {
        // Skip hidden subcommands
        if subcommand.is_hide_set() {
            continue;
        }

        let about = if let Some(about) = subcommand.get_about() {
            about
        } else {
            eprintln!(
                "Warning: Subcommand `{}{}` has no description",
                parent,
                subcommand.get_name()
            );
            exit(1);
        };
        // Create a link to the subcommand's Markdown file
        let command_name = subcommand.get_name();
        let link = format!("{}/{}{}", parent, command_name, MD_EXTENSION);
        let link_md = format!("[`{}`]({})", command_name, link);
        writeln!(buffer, "| {} | {} |", link_md, about,).unwrap();
    }
    buffer
}

// Function to write a list of options to the buffer
fn arguments(options: &[&clap::Arg], parents: &[String]) -> String {
    let mut buffer = String::with_capacity(1024);
    for opt in options {
        if opt.is_hide_set() ||
            // Skip short only as that is a bug in clap's disable help function.
            (opt.get_long().is_none() && opt.get_short().is_some())
        {
            continue;
        }

        let long_name = if let Some(long) = opt.get_long() {
            format!("--{}", long)
        } else if let Some(value_names) = opt.get_value_names() {
            // No long name, but we have value names, assuming positional.
            format!("<{}>", value_names[0])
        } else {
            eprintln!(
                "Error: Option: '{:?}' with parents {} has no long name",
                opt,
                parents.join(" ")
            );
            exit(1);
        };

        // Error on missing help
        if opt.get_help().is_none() && opt.get_long().unwrap() != "help" {
            eprintln!(
                "Error: Option `{} {}` has no description",
                parents.join(" "),
                long_name
            );
            exit(1);
        }

        let id = format!("arg-{}", long_name);

        // Write the option as a bullet point with a self-referential <a> tag
        write!(
            buffer,
            "- <a id=\"{}\" href=\"#{}\">`{}{}{}`</a>\n:",
            id,
            id,
            long_name,
            if let Some(short) = opt.get_short() {
                format!(" (-{})", short)
            } else {
                "".to_string()
            },
            if opt.get_action().takes_values() && !opt.is_positional() {
                if let Some(value_names) = opt.get_value_names() {
                    format!(" <{}>", value_names.join(" "))
                } else {
                    "".to_string()
                }
            } else {
                "".to_string()
            }
        )
        .unwrap();

        // Write the help text
        if let Some(help) = opt.get_help() {
            writeln!(buffer, "  {}", help).unwrap();
        } else {
            writeln!(buffer).unwrap();
        }

        // Write if it might be provided more than once
        if matches!(opt.get_action(), clap::ArgAction::Append) {
            writeln!(buffer, "<br>May be provided more than once.").unwrap();
        }

        // Add required
        if opt.is_required_set() {
            writeln!(buffer, "<br>**required**: `true`").unwrap();
        }

        // Handle aliases, env, defaults, and options
        if let Some(aliases) = opt.get_visible_aliases() {
            if !aliases.is_empty() {
                writeln!(buffer, "<br>**aliases**: {}", aliases.join(", ")).unwrap();
            }
        }
        if let Some(env) = opt.get_env() {
            writeln!(buffer, "<br>**env**: `{}`", env.to_string_lossy()).unwrap();
        }
        if !opt.get_default_values().is_empty() {
            writeln!(
                buffer,
                "<br>**default**: `{}`",
                opt.get_default_values()
                    .iter()
                    .map(|value| {
                        if rattler_conda_types::Platform::from_str(
                            value.as_os_str().to_str().unwrap(),
                        )
                        .is_ok()
                        {
                            "current_platform".to_string()
                        } else {
                            value.to_string_lossy().to_string()
                        }
                    })
                    .join(", ")
            )
            .unwrap();
        }
        if opt.get_action().takes_values() && !opt.get_possible_values().is_empty() {
            writeln!(
                buffer,
                "<br>**options**: `{}`",
                opt.get_possible_values()
                    .iter()
                    .map(|value| value.get_name())
                    .join("`, `")
            )
            .unwrap();
        }
    }
    buffer
}

/// Loads the CLI command structure from pixi
fn get_command() -> Command {
    pixi::cli::Args::command()
}
