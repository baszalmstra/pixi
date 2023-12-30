use std::{collections::HashMap, path::PathBuf, string::String};

use clap::Parser;
use itertools::Itertools;
use miette::{miette, Context, Diagnostic, IntoDiagnostic};
use rattler_conda_types::Platform;

use crate::task::TaskGraph;
use crate::{
    environment::{get_up_to_date_prefix, LockFileUsage},
    prefix::Prefix,
    progress::await_in_progress,
    project::environment::get_metadata_env,
    task::{ExecutableTask, FailedToParseShellScript, InvalidWorkingDirectory, TaskHash},
    Project,
};
use rattler_shell::{
    activation::{ActivationVariables, Activator, PathModificationBehavior},
    shell::ShellEnum,
};
use thiserror::Error;
use tracing::{instrument, Level};

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
}

/// CLI entry point for `pixi run`
/// When running the sigints are ignored and child can react to them. As it pleases.
pub async fn execute(args: Args) -> miette::Result<()> {
    let project = Project::load_or_else_discover(args.manifest_path.as_deref())?;

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
    let task_graph = TaskGraph::from_cmd_args(&project, task_args, Some(Platform::current()))
        .context("failed to construct task graph from command line arguments")?;

    // Traverse the task graph in topological order and execute each individual task.
    let mut task_env = None;
    for task_id in task_graph.topological_order() {
        let executable_task = ExecutableTask::from_task_graph(&task_graph, task_id);

        // Determine the expected hash of the task. This returns `None` if the task does not define
        // any cacheable input.
        let task_hash = TaskHash::from_task(&executable_task).await?;

        // Determine the hash of the previous run of the task. This returns `None` if the task was
        // not previously executed or the cache has been deleted. We only need to check the cache
        // hash if the task has a hash in the first place.
        let previous_hash = if task_hash.is_some() {
            executable_task
                .cached_computation_hash()
                .await
                .into_diagnostic()
                .context("failed to determine the cached computation hash")?
        } else {
            None
        };

        // Determine if the task should be executed or not based on the cached computation hash and
        // the computation hash we computed.
        if let (Some(task_hash), Some(previous_hash)) = (task_hash.as_ref(), previous_hash.as_ref())
        {
            // If the task is up-to-date, we skip it.
            if &task_hash.computation_hash() == previous_hash {
                if tracing::enabled!(Level::WARN) && !executable_task.task().is_custom() {
                    eprintln!(
                        "{}{}{}",
                        console::Emoji("🚀 ", ""),
                        console::style("Skipping (up to date): ").bold(),
                        executable_task.display_command(),
                    );
                }
                continue;
            }
        }

        // If we don't have a command environment yet, we need to compute it. We lazily compute the
        // task environment because we only need the environment if a task is actually executed. If
        // the task is cached, we don't need to compute the environment either.
        let task_env = match task_env.as_ref() {
            None => {
                let env = get_task_env(&project, args.lock_file_usage.into())
                    .await
                    .map_err(|e| TaskExecutionError::FailedToComputeCommandEnv(e.into()))?;
                task_env.insert(env) as &_
            }
            Some(command_env) => command_env,
        };

        // Execute the task itself within the command environment. If one of the tasks failed with
        // a non-zero exit code, we exit this parent process with the same code.
        match execute_task(&executable_task, task_env).await {
            Ok(_) => {}
            Err(TaskExecutionError::NonZeroExitCode(code)) => {
                if code == 127 {
                    command_not_found(&project);
                }
                std::process::exit(code);
            }
            Err(err) => return Err(err.into()),
        }

        // If the execution of the task succeeded, write the hash to the cache.
        if let Some(task_hash) = task_hash {
            // TODO: Update the task hash with the outputs from the task execution.

            executable_task
                .update_cached_computation_hash(&task_hash.computation_hash())
                .await
                .into_diagnostic()
                .context("failed to update the cached computation hash")?;
        }
    }

    Ok(())
}

/// Called when a command was not found.
fn command_not_found(project: &Project) {
    let available_tasks = project
        .tasks(Some(Platform::current()))
        .into_keys()
        .sorted()
        .collect_vec();

    if !available_tasks.is_empty() {
        eprintln!(
            "\nAvailable tasks:\n{}",
            available_tasks.into_iter().format_with("\n", |name, f| {
                f(&format_args!("\t{}", console::style(name).bold()))
            })
        );
    }
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
    #[diagnostic(transparent)]
    FailedToComputeCommandEnv(#[from] Box<dyn Diagnostic + Send + Sync>),
}

/// Called to execute a single command.
///
/// This function is called from [`execute`].
#[instrument(skip(task, command_env), fields(task=%task.cache_name()))]
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

    // Showing which command is being run if the level and type allows it.
    if tracing::enabled!(Level::WARN) && !task.task().is_custom() {
        eprintln!(
            "{} {}{}",
            console::Emoji("✨", "*"),
            console::style("Pixi task: ").bold(),
            task.display_command(),
        );
    }

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

/// Determine the environment variables to use when executing a command. This method runs the
/// activation scripts from the environment and stores the environment variables it added, it adds
/// environment variables set by the project and merges all of that with the system environment
/// variables.
pub async fn get_task_env(
    project: &Project,
    lock_file_usage: LockFileUsage,
) -> miette::Result<HashMap<String, String>> {
    // Get the prefix which we can then activate.
    let prefix = get_up_to_date_prefix(project, lock_file_usage).await?;

    // Get environment variables from the activation
    let activation_env = run_activation_async(project, prefix).await?;

    // Get environment variables from the manifest
    let manifest_env = get_metadata_env(project);

    // Construct command environment by concatenating the environments
    Ok(std::env::vars()
        .chain(activation_env.into_iter())
        .chain(manifest_env.into_iter())
        .collect())
}

/// Runs the activation script asynchronously. This function also adds a progress bar.
pub async fn run_activation_async(
    project: &Project,
    prefix: Prefix,
) -> miette::Result<HashMap<String, String>> {
    let platform = Platform::current();
    let additional_activation_scripts = project.activation_scripts(platform)?;

    // Check if the platform and activation script extension match. For Platform::Windows the extension should be .bat and for All other platforms it should be .sh or .bash.
    for script in additional_activation_scripts.iter() {
        let extension = script.extension().unwrap_or_default();
        if platform.is_windows() && extension != "bat" {
            tracing::warn!("The activation script '{}' does not have the correct extension for the platform '{}'. The extension should be '.bat'.", script.display(), platform);
        } else if !platform.is_windows() && extension != "sh" && extension != "bash" {
            tracing::warn!("The activation script '{}' does not have the correct extension for the platform '{}'. The extension should be '.sh' or '.bash'.", script.display(), platform);
        }
    }

    await_in_progress(
        "activating environment",
        run_activation(prefix, additional_activation_scripts.into_iter().collect()),
    )
    .await
    .wrap_err("failed to activate environment")
}

/// Runs and caches the activation script.
async fn run_activation(
    prefix: Prefix,
    additional_activation_scripts: Vec<PathBuf>,
) -> miette::Result<HashMap<String, String>> {
    let activator_result = tokio::task::spawn_blocking(move || {
        // Run and cache the activation script
        let shell: ShellEnum = ShellEnum::default();

        // Construct an activator for the script
        let mut activator = Activator::from_path(prefix.root(), shell, Platform::current())?;
        activator
            .activation_scripts
            .extend(additional_activation_scripts);

        // Run the activation
        activator.run_activation(ActivationVariables {
            // Get the current PATH variable
            path: Default::default(),

            // Start from an empty prefix
            conda_prefix: None,

            // Prepending environment paths so they get found first.
            path_modification_behaviour: PathModificationBehavior::Prepend,
        })
    })
    .await
    .into_diagnostic()?
    .into_diagnostic()?;

    Ok(activator_result)
}
