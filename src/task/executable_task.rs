use crate::task::{ComputationHash, TaskGraph, TaskId};
use crate::{
    task::{quote_arguments, Task},
    Project,
};
use deno_task_shell::{
    execute_with_pipes, parser::SequentialList, pipe, ShellPipeWriter, ShellState,
};
use miette::Diagnostic;
use std::{
    borrow::Cow,
    collections::HashMap,
    fmt::{Display, Formatter},
    path::PathBuf,
};
use thiserror::Error;
use tokio::fs;
use tokio::task::JoinHandle;

/// Runs task in project.
#[derive(Default, Debug)]
pub struct RunOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Error, Diagnostic)]
#[error("deno task shell failed to parse '{script}': {error}")]
pub struct FailedToParseShellScript {
    pub script: String,
    pub error: String,
}

#[derive(Debug, Error, Diagnostic)]
#[error("invalid working directory '{path}'")]
pub struct InvalidWorkingDirectory {
    pub path: String,
}

#[derive(Debug, Error, Diagnostic)]
pub enum TaskExecutionError {
    #[error(transparent)]
    InvalidWorkingDirectory(#[from] InvalidWorkingDirectory),
    #[error(transparent)]
    FailedToParseShellScript(#[from] FailedToParseShellScript),
}

/// A task that contains enough information to be able to execute it. The lifetime [`'p`] refers to
/// the lifetime of the project that contains the tasks.
#[derive(Clone)]
pub struct ExecutableTask<'p> {
    pub project: &'p Project,
    pub name: Option<String>,
    pub task: Cow<'p, Task>,
    pub additional_args: Vec<String>,
}

impl<'p> ExecutableTask<'p> {
    /// Constructs a new executable task from a task graph node.
    pub fn from_task_graph(task_graph: &TaskGraph<'p>, task_id: TaskId) -> Self {
        let node = &task_graph[task_id];
        Self {
            project: task_graph.project(),
            name: node.name.clone(),
            task: node.task.clone(),
            additional_args: node.additional_args.clone(),
        }
    }

    /// Returns the name of the task or `None` if this is an anonymous task.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Returns the task description from the project.
    pub fn task(&self) -> &Task {
        self.task.as_ref()
    }

    /// Returns any additional args to pass to the execution of the task.
    pub fn additional_args(&self) -> &[String] {
        &self.additional_args
    }

    /// Returns the project in which this task is defined.
    pub fn project(&self) -> &'p Project {
        self.project
    }

    /// Returns a [`SequentialList`] which can be executed by deno task shell. Returns `None` if the
    /// command is not executable like in the case of an alias.
    pub fn as_deno_script(&self) -> Result<Option<SequentialList>, FailedToParseShellScript> {
        // Convert the task into an executable string
        let Some(task) = self.task.as_single_command() else {
            return Ok(None);
        };

        // Append the command line arguments
        let cli_args = quote_arguments(self.additional_args.iter().map(|arg| arg.as_str()));
        let full_script = format!("{task} {cli_args}");

        // Parse the shell command
        deno_task_shell::parser::parse(full_script.trim())
            .map_err(|e| FailedToParseShellScript {
                script: full_script,
                error: e.to_string(),
            })
            .map(Some)
    }

    /// Returns the working directory for this task.
    pub fn working_directory(&self) -> Result<PathBuf, InvalidWorkingDirectory> {
        Ok(match self.task.working_directory() {
            Some(cwd) if cwd.is_absolute() => cwd.to_path_buf(),
            Some(cwd) => {
                let abs_path = self.project.root().join(cwd);
                if !abs_path.is_dir() {
                    return Err(InvalidWorkingDirectory {
                        path: cwd.to_string_lossy().to_string(),
                    });
                }
                abs_path
            }
            None => self.project.root().to_path_buf(),
        })
    }

    /// Returns the full command that should be executed for this task. This includes any
    /// additional arguments that should be passed to the command.
    ///
    /// This function returns `None` if the task does not define a command to execute. This is the
    /// case for alias only commands.
    pub fn full_command(&self) -> Option<String> {
        let mut cmd = self.task.as_single_command()?.to_string();

        if !self.additional_args.is_empty() {
            cmd.push(' ');
            cmd.push_str(&self.additional_args.join(" "));
        }

        Some(cmd)
    }

    /// Returns an object that implements [`Display`] which outputs the command of the wrapped task.
    pub fn display_command(&self) -> impl Display + '_ {
        ExecutableTaskConsoleDisplay { task: self }
    }

    /// Executes the task and capture its output.
    pub async fn execute_with_pipes(
        &self,
        command_env: &HashMap<String, String>,
        input: Option<&[u8]>,
    ) -> Result<RunOutput, TaskExecutionError> {
        let Some(script) = self.as_deno_script()? else {
            return Ok(RunOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        };
        let cwd = self.working_directory()?;
        let (stdin, mut stdin_writer) = pipe();
        if let Some(stdin) = input {
            stdin_writer.write_all(stdin).unwrap();
        }
        drop(stdin_writer); // prevent a deadlock by dropping the writer
        let (stdout, stdout_handle) = get_output_writer_and_handle();
        let (stderr, stderr_handle) = get_output_writer_and_handle();
        let state = ShellState::new(command_env.clone(), &cwd, Default::default());
        let code = execute_with_pipes(script, state, stdin, stdout, stderr).await;
        Ok(RunOutput {
            exit_code: code,
            stdout: stdout_handle.await.unwrap(),
            stderr: stderr_handle.await.unwrap(),
        })
    }

    /// Returns the name of the task or `<anonymous>` if the task doesn't have a name. This is used
    /// for caching purposes.
    pub fn cache_name(&self) -> String {
        self.name
            .to_owned()
            .unwrap_or_else(|| self.display_command().to_string())
    }

    /// Returns the hash of the cache name. This is used to identify the task in caches.
    pub fn cache_name_hash(&self) -> u64 {
        let task_name = self.cache_name();
        xxhash_rust::xxh3::xxh3_64(task_name.as_bytes())
    }

    /// Returns the location of where the task hash is stored.
    pub fn cache_hash_path(&self) -> PathBuf {
        let task_name_hash = self.cache_name_hash();
        self.project
            .root()
            .join(".pixi/cache/task_hashes")
            .join(format!("{:x}", task_name_hash))
    }

    /// Returns the cached [`ComputationHash`] of the task if it exists.
    pub async fn cached_computation_hash(&self) -> std::io::Result<Option<ComputationHash>> {
        match fs::read_to_string(self.cache_hash_path())
            .await
            .map(ComputationHash::from)
        {
            Ok(hash) => Ok(Some(hash)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Writes the [`ComputationHash`] to the cache file on disk.
    pub async fn update_cached_computation_hash(
        &self,
        hash: &ComputationHash,
    ) -> std::io::Result<()> {
        let cache_hash_path = self.cache_hash_path();
        let cache_hash_dir = cache_hash_path
            .parent()
            .expect("cache hash path has no parent");
        fs::create_dir_all(cache_hash_dir).await?;
        fs::write(cache_hash_path, hash.as_str()).await?;
        Ok(())
    }
}

/// A helper object that implements [`Display`] to display (with ascii color) the command of the
/// task.
struct ExecutableTaskConsoleDisplay<'p, 't> {
    task: &'t ExecutableTask<'p>,
}

impl<'p, 't> Display for ExecutableTaskConsoleDisplay<'p, 't> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let command = self.task.full_command();
        write!(
            f,
            "{}",
            console::style(command.as_deref().unwrap_or("<alias>"))
                .blue()
                .bold()
        )
    }
}

/// Helper function to create a pipe that we can get the output from.
fn get_output_writer_and_handle() -> (ShellPipeWriter, JoinHandle<String>) {
    let (reader, writer) = pipe();
    let handle = reader.pipe_to_string_handle();
    (writer, handle)
}
