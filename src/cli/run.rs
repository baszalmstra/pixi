use std::collections::hash_map::Entry;
use std::collections::HashSet;
use std::str::FromStr;
use std::{collections::HashMap, path::PathBuf, string::String};

use clap::Parser;
use itertools::Itertools;
use miette::{miette, Context, Diagnostic};
use rattler_conda_types::Platform;

use crate::activation::get_environment_variables;
use crate::project::errors::UnsupportedPlatformError;
use crate::task::{ExecutableTask, FailedToParseShellScript, InvalidWorkingDirectory, TaskGraph};
use crate::{Project, UpdateLockFileOptions};

use crate::environment::LockFileDerivedData;
use crate::progress::await_in_progress;
use crate::project::manifest::EnvironmentName;
use crate::project::Environment;
use thiserror::Error;
use tracing::Level;

/// Runs task in project.
#[derive(Parser, Debug, Default)]
#[clap(trailing_var_arg = true, arg_required_else_help = true)]
pub struct Args {
    /// The task you want to run in the projects environment.
    pub task: Vec<String>,

    /// The path to 'pixi.toml'
    #[arg(long)]
    pub manifest_path: Option<PathBuf>,

    #[clap(flatten)]
    pub lock_file_usage: super::LockFileUsageArgs,

    #[arg(long, short)]
    pub environment: Option<String>,
}

/// CLI entry point for `pixi run`
/// When running the sigints are ignored and child can react to them. As it pleases.
pub async fn execute(args: Args) -> miette::Result<()> {
    // Load the project
    let project = Project::load_or_else_discover(args.manifest_path.as_deref())?;

    // Extract the passed in environment name.
    let explicit_environment = args
        .environment
        .map(|n| EnvironmentName::from_str(n.as_str()))
        .transpose()?
        .map(|n| {
            project
                .environment(&n)
                .ok_or_else(|| miette::miette!("unknown environment '{n}'"))
        })
        .transpose()?;

    // Ensure that the lock-file is up-to-date.
    let mut lock_file = project
        .up_to_date_lock_file(UpdateLockFileOptions {
            lock_file_usage: args.lock_file_usage.into(),
            ..UpdateLockFileOptions::default()
        })
        .await?;

    // Split 'task' into arguments if it's a single string, supporting commands like:
    // `"test 1 == 0 || echo failed"` or `"echo foo && echo bar"` or `"echo 'Hello World'"`
    // This prevents shell interpretation of pixi run inputs.
    // Use as-is if 'task' already contains multiple elements.
    let task_args = if args.task.len() == 1 {
        shlex::split(args.task[0].as_str())
            .ok_or(miette!("Could not split task, assuming non valid task"))?
    } else {
        args.task
    };
    tracing::debug!("Task parsed from run command: {:?}", task_args);

    // Construct a task graph from the input arguments
    let task_graph = TaskGraph::from_cmd_args(
        &project,
        task_args,
        Some(Platform::current()),
        explicit_environment.clone(),
    )
    .context("failed to construct task graph from command line arguments")?;

    // Traverse the task graph in topological order and execute each individual task.
    let mut task_idx = 0;
    let mut task_envs = HashMap::new();
    for task_id in task_graph.topological_order() {
        let executable_task = ExecutableTask::from_task_graph(&task_graph, task_id);

        // If the task is not executable (e.g. an alias), we skip it. This ensures we don't
        // instantiate a prefix for an alias.
        if !executable_task.task().is_executable() {
            continue;
        }

        // Showing which command is being run if the level and type allows it.
        if tracing::enabled!(Level::WARN) && !executable_task.task().is_custom() {
            if task_idx > 0 {
                // Add a newline between task outputs
                eprintln!();
            }
            eprintln!(
                "{}{}{}{}{}",
                console::Emoji("✨ ", ""),
                console::style("Pixi task (").bold(),
                console::style(executable_task.run_environment.name())
                    .bold()
                    .cyan(),
                console::style("): ").bold(),
                executable_task.display_command(),
            );
        }

        // If we don't have a command environment yet, we need to compute it. We lazily compute the
        // task environment because we only need the environment if a task is actually executed.
        let task_env: &_ = match task_envs.entry(executable_task.run_environment.clone()) {
            Entry::Occupied(env) => env.into_mut(),
            Entry::Vacant(entry) => {
                let command_env =
                    get_task_env(&mut lock_file, &executable_task.run_environment).await?;
                entry.insert(command_env)
            }
        };

        // Execute the task itself within the command environment. If one of the tasks failed with
        // a non-zero exit code, we exit this parent process with the same code.
        match execute_task(&executable_task, task_env).await {
            Ok(_) => {
                task_idx += 1;
            }
            Err(TaskExecutionError::NonZeroExitCode(code)) => {
                if code == 127 {
                    command_not_found(&project, explicit_environment);
                }
                std::process::exit(code);
            }
            Err(err) => return Err(err.into()),
        }
    }

    Ok(())
}

/// Called when a command was not found.
fn command_not_found<'p>(project: &'p Project, explicit_environment: Option<Environment<'p>>) {
    let available_tasks: HashSet<String> = if let Some(explicit_environment) = explicit_environment
    {
        explicit_environment
            .tasks(Some(Platform::current()))
            .into_iter()
            .flat_map(|tasks| tasks.into_keys())
            .map(ToOwned::to_owned)
            .collect()
    } else {
        project
            .environments()
            .into_iter()
            .flat_map(|env| {
                env.tasks(Some(Platform::current()))
                    .into_iter()
                    .flat_map(|tasks| tasks.into_keys())
                    .map(ToOwned::to_owned)
            })
            .collect()
    };

    if !available_tasks.is_empty() {
        eprintln!(
            "\nAvailable tasks:\n{}",
            available_tasks
                .into_iter()
                .sorted()
                .format_with("\n", |name, f| {
                    f(&format_args!("\t{}", console::style(name).bold()))
                })
        );
    }
}

/// Determine the environment variables to use when executing a command. The method combines the
/// activation environment with the system environment variables.
pub async fn get_task_env<'p>(
    lock_file_derived_data: &mut LockFileDerivedData<'p>,
    environment: &Environment<'p>,
) -> miette::Result<HashMap<String, String>> {
    // Ensure there is a valid prefix
    lock_file_derived_data.prefix(environment).await?;

    // Get environment variables from the activation
    let activation_env = await_in_progress("activating environment", |_| {
        crate::activation::run_activation(environment)
    })
    .await
    .wrap_err("failed to activate environment")?;

    // Get environments from pixi
    let environment_variables = get_environment_variables(environment);

    // Concatenate with the system environment variables
    Ok(std::env::vars()
        .chain(activation_env)
        .chain(environment_variables)
        .collect())
}

#[derive(Debug, Error, Diagnostic)]
enum TaskExecutionError {
    #[error("the script exited with a non-zero exit code {0}")]
    NonZeroExitCode(i32),

    #[error(transparent)]
    FailedToParseShellScript(#[from] FailedToParseShellScript),

    #[error(transparent)]
    InvalidWorkingDirectory(#[from] InvalidWorkingDirectory),

    #[error(transparent)]
    UnsupportedPlatformError(#[from] UnsupportedPlatformError),
}

/// Called to execute a single command.
///
/// This function is called from [`execute`].
async fn execute_task<'p>(
    task: &ExecutableTask<'p>,
    command_env: &HashMap<String, String>,
) -> Result<(), TaskExecutionError> {
    let Some(script) = task.as_deno_script()? else {
        return Ok(());
    };
    let cwd = task.working_directory()?;

    // Ignore CTRL+C
    // Specifically so that the child is responsible for its own signal handling
    // NOTE: one CTRL+C is registered it will always stay registered for the rest of the runtime of the program
    // which is fine when using run in isolation, however if we start to use run in conjunction with
    // some other command we might want to revaluate this.
    let ctrl_c = tokio::spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });

    let execute_future =
        deno_task_shell::execute(script, command_env.clone(), &cwd, Default::default());
    let status_code = tokio::select! {
        code = execute_future => code,
        // This should never exit
        _ = ctrl_c => { unreachable!("Ctrl+C should not be triggered") }
    };

    if status_code != 0 {
        return Err(TaskExecutionError::NonZeroExitCode(status_code));
    }

    Ok(())
}
