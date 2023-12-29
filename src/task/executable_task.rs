use crate::task::file_hashes::{FileHashes, FileHashesError};
use crate::task::task_hash::TaskHash;
use crate::{
    task::{quote_arguments, CmdArgs, Custom, Task},
    Project,
};
use deno_task_shell::{
    execute_with_pipes, parser::SequentialList, pipe, ShellPipeWriter, ShellState,
};
use miette::Diagnostic;
use rattler_conda_types::Platform;
use std::{
    borrow::Cow,
    collections::HashMap,
    env,
    fmt::{Display, Formatter},
    path::PathBuf,
};
use thiserror::Error;
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
    pub(super) project: &'p Project,
    pub(super) name: Option<String>,
    pub(super) task: Cow<'p, Task>,
    pub(super) additional_args: Vec<String>,
    pub(super) platform: Option<Platform>,
}

impl<'p> ExecutableTask<'p> {
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

    /// Parses command line arguments into an [`ExecutableTask`].
    pub fn from_cmd_args(
        project: &'p Project,
        args: Vec<String>,
        platform: Option<Platform>,
    ) -> Self {
        let mut args = args;

        if let Some(name) = args.first() {
            // Find the task in the project. First searches for platform specific tasks and falls
            // back to looking for the task in the default tasks.
            if let Some(task) = project.task_opt(name, platform) {
                return Self {
                    project,
                    name: Some(args.remove(0)),
                    task: Cow::Borrowed(task),
                    additional_args: args,
                    platform,
                };
            }
        }

        // When no task is found, just execute the command verbatim.
        Self {
            project,
            name: None,
            task: Cow::Owned(
                Custom {
                    cmd: CmdArgs::from(args),
                    cwd: env::current_dir().ok(),
                }
                .into(),
            ),
            additional_args: vec![],
            platform,
        }
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

    /// Determines if the task is up-to-date and doesn't need regeneration.
    pub async fn is_up_to_date(&self) -> Result<bool, FileHashesError> {
        // Read the current task hash
        let cache_hash_path = self.cache_hash_path();
        let previous_task_hash = match tokio::fs::read_to_string(&cache_hash_path).await {
            Ok(hash) => hash,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // The task hash file doesn't exist. Not up to date.
                return Ok(false);
            }
            Err(err) => return Err(FileHashesError::IoError(cache_hash_path, err)),
        };

        // Compute the task hash
        let Some(current_task_hash) = self.hash().await? else {
            // No valid task hash, not up to date.
            return Ok(false);
        };

        // Compare the task hash with the existing hash
        if previous_task_hash == current_task_hash.hash() {
            // The task is up to date.
            return Ok(true);
        }

        // The task is out of date.
        Ok(false)
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

    /// Compute the task hash
    pub async fn hash(&self) -> Result<Option<TaskHash>, FileHashesError> {
        let Some(cache) = self.task.cache() else {
            return Ok(None);
        };

        let Some(inputs) = cache.inputs.as_ref() else {
            return Ok(None);
        };

        Ok(Some(TaskHash {
            inputs: FileHashes::from_files(self.project.root(), inputs).await?,
            name: self.cache_name().to_string(),
        }))
    }
}

/// A helper object that implements [`Display`] to display (with ascii color) the command of the
/// task.
struct ExecutableTaskConsoleDisplay<'p, 't> {
    task: &'t ExecutableTask<'p>,
}

impl<'p, 't> Display for ExecutableTaskConsoleDisplay<'p, 't> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let command = self.task.task.as_single_command();
        write!(
            f,
            "{}",
            console::style(command.as_deref().unwrap_or("<alias>"))
                .blue()
                .bold()
        )?;
        if !self.task.additional_args.is_empty() {
            write!(
                f,
                " {}",
                console::style(self.task.additional_args.join(" ")).blue()
            )?;
        }
        Ok(())
    }
}
/// Helper function to create a pipe that we can get the output from.
fn get_output_writer_and_handle() -> (ShellPipeWriter, JoinHandle<String>) {
    let (reader, writer) = pipe();
    let handle = reader.pipe_to_string_handle();
    (writer, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn test_ordered_commands() {
        let file_content = r#"
        [project]
        name = "pixi"
        channels = ["conda-forge"]
        platforms = ["linux-64"]
        [tasks]
        root = "echo root"
        task1 = {cmd="echo task1", depends_on=["root"]}
        task2 = {cmd="echo task2", depends_on=["root"]}
        top = {cmd="echo top", depends_on=["task1","task2"]}
    "#;
        let project = Project::from_manifest_str(Path::new(""), file_content.to_string()).unwrap();

        let executable_tasks = ExecutableTask::from_cmd_args(
            &project,
            vec!["top".to_string(), "--test".to_string()],
            Some(Platform::current()),
        )
        .get_ordered_dependencies()
        .await
        .unwrap();

        let ordered_task_names: Vec<_> = executable_tasks
            .iter()
            .map(|task| task.task().as_single_command().unwrap())
            .collect();

        assert_eq!(
            ordered_task_names,
            vec!["echo root", "echo task1", "echo task2", "echo top"]
        );

        // Also check if the arguments are passed correctly
        assert_eq!(
            executable_tasks.last().unwrap().additional_args(),
            vec!["--test".to_string()]
        );
    }

    #[tokio::test]
    async fn test_cycle_ordered_commands() {
        let file_content = r#"
        [project]
        name = "pixi"
        channels = ["conda-forge"]
        platforms = ["linux-64"]
        [tasks]
        root = {cmd="echo root", depends_on=["task1"]}
        task1 = {cmd="echo task1", depends_on=["root"]}
        task2 = {cmd="echo task2", depends_on=["root"]}
        top = {cmd="echo top", depends_on=["task1","task2"]}
    "#;
        let project = Project::from_manifest_str(Path::new(""), file_content.to_string()).unwrap();

        let executable_tasks = ExecutableTask::from_cmd_args(
            &project,
            vec!["top".to_string()],
            Some(Platform::current()),
        )
        .get_ordered_dependencies()
        .await
        .unwrap();

        let ordered_task_names: Vec<_> = executable_tasks
            .iter()
            .map(|task| task.task().as_single_command().unwrap())
            .collect();

        assert_eq!(
            ordered_task_names,
            vec!["echo root", "echo task1", "echo task2", "echo top"]
        );
    }

    #[tokio::test]
    async fn test_platform_ordered_commands() {
        let file_content = r#"
        [project]
        name = "pixi"
        channels = ["conda-forge"]
        platforms = ["linux-64"]
        [tasks]
        root = "echo root"
        task1 = {cmd="echo task1", depends_on=["root"]}
        task2 = {cmd="echo task2", depends_on=["root"]}
        top = {cmd="echo top", depends_on=["task1","task2"]}
        [target.linux-64.tasks]
        root = {cmd="echo linux", depends_on=["task1"]}
    "#;
        let project = Project::from_manifest_str(Path::new(""), file_content.to_string()).unwrap();

        let executable_tasks = ExecutableTask::from_cmd_args(
            &project,
            vec!["top".to_string()],
            Some(Platform::Linux64),
        )
        .get_ordered_dependencies()
        .await
        .unwrap();

        let ordered_task_names: Vec<_> = executable_tasks
            .iter()
            .map(|task| task.task().as_single_command().unwrap())
            .collect();

        assert_eq!(
            ordered_task_names,
            vec!["echo linux", "echo task1", "echo task2", "echo top",]
        );
    }

    #[tokio::test]
    async fn test_custom_command() {
        let file_content = r#"
        [project]
        name = "pixi"
        channels = ["conda-forge"]
        platforms = ["linux-64"]
    "#;
        let project = Project::from_manifest_str(Path::new(""), file_content.to_string()).unwrap();

        let executable_tasks = ExecutableTask::from_cmd_args(
            &project,
            vec!["echo bla".to_string()],
            Some(Platform::Linux64),
        )
        .get_ordered_dependencies()
        .await
        .unwrap();

        assert_eq!(executable_tasks.len(), 1);

        let task = executable_tasks.get(0).unwrap();
        assert!(task.task().is_custom());

        assert_eq!(
            task.task().as_single_command().unwrap(),
            r###""echo bla""###
        );
    }
}
